// 仅在网页模型中换算实物参考角。角差跨越 ±π 时仍取最短弧。
const deg = 180 / Math.PI;
const separation = 5 / deg;
const arc = value => Math.atan2(Math.sin(value), Math.cos(value));

export function fitPose(binding, first, second) {
  const motion = arc(second.motor - first.motor);
  if (!Number.isFinite(motion) || Math.abs(motion) < separation)
    throw new Error('两次反馈相差不到 5°，请选两个明显不同的姿态。');
  if (binding.range) {
    if (Math.PI - Math.abs(motion) < separation) throw new Error('夹爪端点接近半圈，请核对所选关节和两个端点。');
    return {motor_range: [first.motor, second.motor]};
  }
  const reference = arc(second.reference - first.reference);
  if (!Number.isFinite(reference) || Math.abs(reference) < separation
      || Math.PI - Math.abs(reference) < separation || Math.PI - Math.abs(motion) < separation)
    throw new Error('这两个角度无法区分正反方向，请选不同且不相差半圈的姿态。');
  const sign = Math.sign(motion) === Math.sign(reference) ? 1 : -1;
  const a = arc(first.reference - sign*first.motor);
  const b = arc(second.reference - sign*second.motor);
  return {sign, offset: arc(a + arc(b-a)/2)};
}

async function request(path, method = 'GET', data, timeoutMs = 8000) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const result = await fetch('/api/calibration/' + path, {
      method, mode: 'same-origin', signal: controller.signal, cache: 'no-store',
      headers: data ? {'Content-Type':'application/json'} : {},
      body: data ? JSON.stringify(data) : undefined,
    });
    const value = await result.json();
    if (!result.ok) {
      const failure = new Error(value.error || '请求失败（HTTP ' + result.status + '）。');
      failure.reply = value;
      throw failure;
    }
    return value;
  } finally {
    clearTimeout(timeout);
  }
}

const motorSteps = {
  ready: ['校准已就绪。手摆到参考中位，停稳后点击标零，写入电机 EEPROM。', 'center', '标零（写入电机）'],
  centered: ['已标零。手动摆到最大或最小位置，停稳后记录。', 'sample1', '记录第一个端点'],
  sampled1: ['请手动摆到另一端，停稳后记录。', 'sample2', '记录另一个端点'],
  sampled2: ['请核对两个端点，再写入电机并保存此关节校准。', 'save', '写入限位并保存'],
  saved: ['电机校准已保存。'],
  cancelled: ['会话已取消。'],
  restored: ['原校准已恢复。'],
  recovery: ['需要恢复原校准，保持断扭和支撑。', 'restore', '恢复原电机校准'],
};

export function createCalibrationPanel(getSample, display, feedbackAngle) {
  const ids = ['model', 'joint', 'model-step', 'angle-label', 'angle', 'measures', 'model-result',
    'record', 'save', 'discard', 'default', 'motor', 'start', 'motor-status', 'motor-readings', 'refresh', 'next', 'cancel'];
  const ui = Object.fromEntries(ids.map(id => [id, document.getElementById('cal-' + id)]));
  let overrides = {}, loaded = false, modelBusy = false;
  let selection = '', sourceSession, points = [], candidate = null;
  let motor = null, motorBusy = false, motorEpoch = 0, motorMessage = '', poll;

  function showMapping() {
    const merged = {...overrides};
    if (candidate) merged[getSample().binding.joint] = candidate;
    display(merged);
  }
  function clearDraft() {
    points = [];
    candidate = null;
    ui.angle.value = '';
    ui.measures.textContent = '';
    ui['model-result'].textContent = '';
  }
  function freshContext() {
    const context = getSample();
    const {binding, bus, joint, session, fresh} = context;
    const key = [binding?.joint, bus?.robot_id, bus?.hardware_id, bus?.layout_id, joint?.motor_id].join('/');
    if (selection !== key || sourceSession !== session || (!fresh && points.length)) {
      const hadPreview = Boolean(candidate);
      const lost = !fresh && points.length;
      clearDraft();
      selection = key;
      sourceSession = session;
      if (lost) ui['model-result'].textContent = '反馈已中断，请重新采样。';
      if (hadPreview) display(overrides);
    }
    return context;
  }

  function refresh() {
    const {binding, fresh, label} = freshContext();
    if (ui.motor.open) renderMotor();
    if (!binding || !ui.model.open) return;
    const stage = points.length;
    const jaw = Boolean(binding.range);
    ui.joint.textContent = (label || binding.joint) + ' · ' + binding.bus;
    const steps = jaw
      ? ['轻合夹爪，记录闭合位置。', '张开夹爪，记录张开位置。', '预览已生效。将夹爪摆到中间，输入实际张角复核。']
      : ['摆到第一个可量取的姿态，输入相对 URDF 零位的实物角度。', '摆到另一个可量取的姿态，输入同一方向约定下的角度。', '预览已生效。摆到第三个姿态，输入实际角度复核。'];
    ui['model-step'].textContent = !loaded ? '正在读取或等待显示配置。'
      : !fresh ? '连接状态流，等待本关节的新鲜位置反馈。'
      : stage < 3 ? steps[stage] : '请对照实物和预览，核对下方复核误差。';
    ui['angle-label'].hidden = stage === 3 || (jaw && stage < 2);
    ui.record.textContent = stage === 2 ? '记录复核姿态' : jaw ? (stage ? '记录张开' : '记录闭合') : '记录姿态 ' + (stage+1);
    ui.record.hidden = stage === 3;
    ui.record.disabled = !loaded || modelBusy || !fresh;
    ui.save.hidden = stage !== 3;
    ui.save.disabled = modelBusy || !fresh;
    ui.discard.hidden = !stage;
    ui.discard.disabled = modelBusy;
    ui.default.disabled = modelBusy || !loaded || !Object.hasOwn(overrides, binding.joint);
  }

  async function load() {
    if (modelBusy) return;
    modelBusy = true;
    try {
      const result = await request('model');
      overrides = result.overrides;
      loaded = true;
      showMapping();
    } catch (err) {
      loaded = false;
      ui['model-result'].textContent = '无法读取已存显示校准，请重新展开此栏。' + err.message;
    } finally {
      modelBusy = false;
      refresh();
    }
  }
  ui.model.addEventListener('toggle', () => {
    if (ui.model.open) { void load(); refresh(); }
    else { clearDraft(); showMapping(); }
  });
  ui.record.addEventListener('click', () => {
    const {binding, bus, joint, fresh} = freshContext();
    if (!ui.model.open || !fresh || !loaded || modelBusy || points.length >= 3) return;
    try {
      if (points.some(point => BigInt(bus.stamp_ns) <= point.stamp))
        throw new Error('请等待下一帧新反馈再记录。');
      const motor = joint.position_rad;
      const stage = points.length;
      const reference = binding.range && stage < 2 ? binding.range[stage] : ui.angle.valueAsNumber / deg;
      if (!Number.isFinite(reference) || Math.abs(reference) > 2*Math.PI)
        throw new Error('请输入 −360° 至 360° 的实测角度。');
      const next = {motor, reference, stamp: BigInt(bus.stamp_ns)};
      if (stage === 1) candidate = fitPose(binding, points[0], next);
      if (stage === 2) {
        if (points.some(point => Math.abs(arc(motor - point.motor)) < separation))
          throw new Error('请换到与前两次均不同的姿态，再记录复核值。');
        if (binding.range) {
          const angle = feedbackAngle({...binding, displayMapping:candidate}, bus, joint);
          if (angle === null || binding.range.includes(angle))
            throw new Error('请把夹爪停在已记录的两个端点之间，再复核实际张角。');
        }
      }
      points.push(next);
      ui.angle.value = '';
      ui.measures.textContent = points.map((point, i) =>
        (i === 2 ? '复核' : '姿态 ' + (i+1)) + '：反馈 ' + (point.motor*deg).toFixed(1)
        + '° / 参考 ' + (point.reference*deg).toFixed(1) + '°'
      ).join('　');
      ui['model-result'].textContent = stage === 2
        ? '模型相对实物偏差 ' + (arc(feedbackAngle({...binding, displayMapping:candidate}, bus, joint)-reference)*deg).toFixed(1) + '°。'
        : stage === 1 ? '当前画布正在预览这次校准。' : '';
      if (candidate) showMapping();
    } catch (err) { ui['model-result'].textContent = err.message; }
    refresh();
  });
  ui.discard.addEventListener('click', () => { clearDraft(); showMapping(); refresh(); });

  async function store(reset) {
    const {binding, fresh} = freshContext();
    if (!binding || modelBusy || !loaded || (!reset && (!candidate || !fresh || points.length !== 3))) return;
    const change = {joint: binding.joint, value: reset ? null : candidate};
    modelBusy = true;
    refresh();
    try {
      const result = await request('model', 'PUT', change);
      overrides = result.overrides;
      if (getSample().binding?.joint === change.joint) {
        clearDraft();
        ui['model-result'].textContent = reset ? '已恢复本关节默认显示。' : '已保存本关节显示校准。';
      }
      showMapping();
    } catch (err) {
      loaded = false;
      clearDraft();
      showMapping();
      ui['model-result'].textContent = '保存结果待核对，请重新展开此栏读取。' + err.message;
    } finally { modelBusy = false; refresh(); }
  }
  ui.save.addEventListener('click', () => void store(false));
  ui.default.addEventListener('click', () => void store(true));

  const handoffLabels = {
    idle: '点击开启时会交接所选侧总线，随后在这里给单个电机标零。',
    starting: '正在交接该侧总线，请等待。',
    serving: '维护会话正在交接，请读取维护状态。若维护已中断，可重新开启同一关节以恢复。',
    returning: '维护会话已结束，正在归还该侧总线。',
    returned: '该侧已恢复保护待机。',
    failed: '维护交接未完成。',
  };
  const terminal = ['saved', 'restored', 'cancelled'];
  function renderMotor() {
    const {binding, label, bus} = getSample();
    const selected = binding && motor?.bus === binding.bus && motor?.joint === binding.joint;
    const step = Object.hasOwn(motorSteps, motor?.phase) ? motorSteps[motor.phase] : null;
    const reconnect = ['starting','serving','returning'].includes(motor?.phase);
    const startable = ['idle','failed','returned'].includes(motor?.phase) || (reconnect && selected);
    ui.start.textContent = reconnect ? '重新接入所选电机校准' : '开启所选电机校准';
    ui.start.hidden = !startable || !binding
      || bus?.control?.enabled === false;
    ui.start.disabled = motorBusy;
    ui.refresh.disabled = motorBusy;
    ui.next.hidden = !selected || !step?.[1];
    ui.next.textContent = step?.[2] || '';
    ui.next.disabled = motorBusy;
    ui.cancel.hidden = !selected || !['ready', 'centered', 'sampled1', 'sampled2'].includes(motor?.phase);
    ui.cancel.textContent = motor?.phase === 'ready' ? '取消会话' : '撤销校准并恢复原值';
    ui.cancel.disabled = motorBusy;
    const target = motor?.joint ? motor.bus + ' · ' + motor.joint : label || binding?.joint || '';
    const hint = step && !selected ? '请先选中本会话的关节，再操作。' : '';
    const outcome = terminal.includes(motor?.calibration_result) ? motorSteps[motor.calibration_result][0] : '';
    ui['motor-status'].textContent = [target, motorMessage, outcome, motor?.error, motor?.position_warning, hint,
      step?.[0] || handoffLabels[motor?.phase] || (!motorMessage && '等待读取维护状态。')].filter(Boolean).join('　');
    const readings = [['position','当前位置'],['center','中位'],['sample1','第一端'],['sample2','第二端']];
    const values = readings.filter(([key]) => Number.isSafeInteger(motor?.[key]))
      .map(([key,label]) => label + ' ' + motor[key] + ' raw');
    if (Number.isSafeInteger(motor?.sample1) && Number.isSafeInteger(motor?.sample2)) {
      values.push('限位范围 ' + Math.min(motor.sample1, motor.sample2) + ' 至 ' + Math.max(motor.sample1, motor.sample2) + ' raw');
    }
    ui['motor-readings'].textContent = values.join('　');
  }
  function validMotorState(value) {
    return value && (Object.hasOwn(handoffLabels, value.phase) || (Object.hasOwn(motorSteps, value.phase)
      && value.session && value.joint && ['bus1','bus2'].includes(value.bus)));
  }
  async function motorRequest(action, background = false) {
    if (motorBusy || !ui.motor.open || document.hidden) return;
    const {binding, label} = getSample();
    const starting = action === 'start';
    const writing = Boolean(action) && !starting;
    const previous = motor;
    if (starting && (!binding || ui.start.hidden || !window.confirm(
      '开启 ' + binding.bus + ' · ' + (label || binding.joint) + ' 的校准？请确认机械臂已支撑，该侧已断扭、急停锁存。此侧控制服务会交出串口。'))) return;
    if (writing && (!motor || binding?.joint !== motor.joint || binding?.bus !== motor.bus
      || !(action === motorSteps[motor.phase]?.[1] || (action === 'cancel' && !ui.cancel.hidden)))) return;

    clearTimeout(poll);
    const epoch = ++motorEpoch;
    // 读回在途时仍接受现场点击，后返回的旧读数由 epoch 丢弃。
    motorBusy = !background;
    if (!background) motorMessage = starting ? '正在交接所选侧总线，请等待…' : action ? '等待维护会话回执…' : '正在读取维护状态…';
    renderMotor();
    try {
      const result = starting
        ? await request('motor/start', 'POST', {bus:binding.bus, joint:binding.joint, supported:true}, 80000)
        : await request('motor', action ? 'POST' : 'GET', action ? {action, session:previous.session, bus:previous.bus} : undefined);
      if (epoch !== motorEpoch) return;
      if (!validMotorState(result)
        || (starting && (result.bus !== binding.bus || result.joint !== binding.joint))
        || (writing && (result.session !== previous.session || result.bus !== previous.bus || result.joint !== previous.joint)))
        throw new Error('会话状态不匹配。');
      motor = result;
      motorMessage = '';
    } catch (err) {
      if (epoch !== motorEpoch) return;
      const reply = err.reply;
      // 校准拒绝也可能带回 recovery，保留同一会话的恢复按钮及原始原因。
      const known = validMotorState(reply) && (!action || (writing && previous?.session
        && reply.session === previous.session && reply.bus === previous.bus && reply.joint === previous.joint));
      motor = known ? reply : null;
      motorMessage = known ? '' : (action ? '结果待核对，请读取维护状态。' : '读取失败。') + err.message;
    } finally {
      if (epoch === motorEpoch) {
        motorBusy = false;
        renderMotor();
        if (ui.motor.open && !document.hidden) {
          if (['centered','sampled1','sampled2'].includes(motor?.phase))
            poll = setTimeout(() => void motorRequest(undefined, true), 500);
          else if (terminal.includes(motor?.phase) || ['starting','returning'].includes(motor?.phase))
            poll = setTimeout(() => void motorRequest(undefined, true), 2000);
        }
      }
    }
  }
  function forgetMotor() {
    clearTimeout(poll);
    motorEpoch++;
    motor = null;
    motorBusy = false;
    motorMessage = '';
    renderMotor();
  }
  ui.motor.addEventListener('toggle', () => {
    if (ui.motor.open) void motorRequest(); else forgetMotor();
  });
  ui.start.addEventListener('click', () => void motorRequest('start'));
  ui.refresh.addEventListener('click', () => void motorRequest());
  ui.next.addEventListener('click', event => {
    // 同一位置的按钮会推进到下一步，双击只能确认一次。
    if (event.detail <= 1) void motorRequest(motorSteps[motor?.phase]?.[1]);
  });
  ui.cancel.addEventListener('click', () => void motorRequest('cancel'));

  function leave() {
    ui.model.open = ui.motor.open = false;
    clearDraft();
    forgetMotor();
    display(overrides);
  }
  return {load, refresh, leave};
}
