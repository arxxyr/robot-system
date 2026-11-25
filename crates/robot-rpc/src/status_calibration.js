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

async function request(path, method = 'GET', data) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 8000);
  try {
    const result = await fetch('/api/calibration/' + path, {
      method, mode: 'same-origin', signal: controller.signal, cache: 'no-store',
      headers: data ? {'Content-Type':'application/json'} : {},
      body: data ? JSON.stringify(data) : undefined,
    });
    const value = await result.json();
    if (!result.ok) throw new Error(value.error || '请求失败（HTTP ' + result.status + '）。');
    return value;
  } finally {
    clearTimeout(timeout);
  }
}

const motorSteps = {
  ready: ['支撑关节，手摆到中位。', 'center', '确认中位并写电机零位'],
  centered: ['手动摆到第一个端点。', 'sample1', '记录第一端'],
  sampled1: ['手动摆到另一端。', 'sample2', '记录第二端'],
  sampled2: ['请核对两个端点，再写入电机并保存此关节校准。', 'save', '写入限位并保存'],
  saved: ['电机校准已保存。'],
  cancelled: ['会话已取消。'],
  restored: ['原校准已恢复。'],
  recovery: ['需要恢复原校准，保持断扭和支撑。', 'restore', '恢复原电机校准'],
};

export function createCalibrationPanel(getSample, display, feedbackAngle) {
  const ids = ['model', 'joint', 'model-step', 'angle-label', 'angle', 'measures', 'model-result',
    'record', 'save', 'discard', 'default', 'motor', 'motor-status', 'motor-readings', 'refresh', 'next', 'cancel'];
  const ui = Object.fromEntries(ids.map(id => [id, document.getElementById('cal-' + id)]));
  let overrides = {}, loaded = false, modelBusy = false;
  let selection = '', sourceSession, points = [], candidate = null;
  let motor = null, motorBusy = false, motorEpoch = 0;

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

  function renderMotor(message) {
    const step = motorSteps[motor?.phase];
    ui.refresh.disabled = motorBusy;
    ui.next.hidden = !step?.[1];
    ui.next.textContent = step?.[2] || '';
    ui.next.disabled = motorBusy;
    ui.cancel.hidden = !motor || !['ready', 'centered', 'sampled1', 'sampled2'].includes(motor.phase);
    ui.cancel.textContent = motor?.phase === 'ready' ? '取消会话' : '撤销校准并恢复原值';
    ui.cancel.disabled = motorBusy;
    const title = motor ? motor.bus + ' · ' + motor.joint + '　' : '';
    ui['motor-status'].textContent = title + (message || step?.[0] || '等待查询维护会话。');
    const readings = [['position','当前位置'],['center','中位'],['sample1','第一端'],['sample2','第二端']];
    ui['motor-readings'].textContent = readings.filter(([key]) => Number.isSafeInteger(motor?.[key]))
      .map(([key,label]) => label + ' ' + motor[key] + ' raw').join('　');
  }
  async function motorRequest(action) {
    if (motorBusy || !ui.motor.open) return;
    if (action && (!motor || !(action === motorSteps[motor.phase]?.[1] || (action === 'cancel' && !ui.cancel.hidden)))) return;
    const previous = motor;
    const epoch = ++motorEpoch;
    motorBusy = true;
    renderMotor(action ? '等待维护会话回执…' : '正在读取维护状态…');
    try {
      const result = await request('motor', action ? 'POST' : 'GET', action ? {action, session:previous.session, bus:previous.bus} : undefined);
      if (epoch !== motorEpoch) return;
      if (!Object.hasOwn(motorSteps, result.phase) || !result.session || !result.joint ||
          !['bus1','bus2'].includes(result.bus) || (action && (result.session !== previous.session || result.bus !== previous.bus || result.joint !== previous.joint)))
        throw new Error('会话状态不匹配。');
      motor = result;
      renderMotor(result.ok ? null : result.error || '操作失败，请核对维护状态。');
    } catch (err) {
      if (epoch !== motorEpoch) return;
      motor = null;
      renderMotor((action ? '结果待核对，请读取维护状态。' : '无法读取维护会话。') + err.message);
    } finally {
      if (epoch === motorEpoch) {
        motorBusy = false;
        ui.refresh.disabled = ui.next.disabled = ui.cancel.disabled = false;
      }
    }
  }
  function forgetMotor() {
    motorEpoch++;
    motor = null;
    motorBusy = false;
    renderMotor();
  }
  ui.motor.addEventListener('toggle', () => {
    if (ui.motor.open) void motorRequest(); else forgetMotor();
  });
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
