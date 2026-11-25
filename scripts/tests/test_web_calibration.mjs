// 浏览器资源是 ES 模块，按它们在网页中的模块语义载入，无需 npm 依赖。
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {test} from 'node:test';

async function asset(name) {
  const source = await readFile(new URL('../../crates/robot-rpc/src/' + name, import.meta.url), 'utf8');
  return import('data:text/javascript;base64,' + Buffer.from(source).toString('base64'));
}

const {createCalibrationPanel, fitPose} = await asset('status_calibration.js');
const {feedbackAngle} = await asset('status_pose.js');
const deg = Math.PI / 180;
const near = (a, b) => assert.ok(Math.abs(a-b) < 1e-8, [a,b].join(' ≠ '));
const point = (motor, reference) => ({motor: motor*deg, reference: reference*deg});

test('旋转关节在跨零姿态判断正反方向并按第三个角度验证', () => {
  for (const sign of [-1, 1]) {
    const value = fitPose({}, point(170, sign*170+10), point(-140, sign*-140+10));
    assert.equal(value.sign, sign);
    near(value.offset, 10*deg);
    near(feedbackAngle({displayMapping:value}, {}, {mode:'position', position_rad:0.7}), sign*0.7+10*deg);
  }
  assert.throws(() => fitPose({}, point(20, 10), point(20.1, 20)));
  assert.throws(() => fitPose({}, point(20, 10), point(-160, -170)));
});

test('夹爪闭合和张开可对应递减编码器，中间点沿 URDF 张角插值', () => {
  const jaw = {range:[0, 1.7]};
  const value = fitPose(jaw, point(90, 0), point(-30, 97));
  const angle = (mapping, motor) => feedbackAngle({...jaw, displayMapping:mapping}, {}, {mode:'position', position_rad:motor*deg});
  near(angle(value, 30), 0.85);
  near(angle(value, 93), 0);
  near(angle(value, -32), 1.7);
  const seam = fitPose(jaw, point(170, 0), point(-150, 97));
  near(angle(seam, -170), 0.85);
  near(angle(seam, -140), 1.7);
  near(angle(seam, 165), 0);
  const beforeClosed = {mode:'position', position_rad:0, position_raw:90, target_range_raw:[100,1000]};
  near(feedbackAngle(jaw, {}, beforeClosed), 0);
  near(feedbackAngle(jaw, {}, {...beforeClosed, position_raw:1001}), 1.7);
  assert.equal(beforeClosed.position_raw, 90);
});

test('页面丢失响应后只允许查询维护事实，过期反馈撤销模型预览', async () => {
  const elements = new Map();
  function element(id) {
    if (!elements.has(id)) elements.set(id, {listeners:{}, open:false,
      addEventListener(type, cb) { this.listeners[type] = cb; },
      async fire(type) { this.listeners[type]({detail:1}); await new Promise(resolve => setImmediate(resolve)); },
    });
    return elements.get(id);
  }
  const doc = globalThis.document, originalFetch = globalThis.fetch;
  globalThis.document = {getElementById:element};
  const el = id => element('cal-' + id);
  const binding = {joint:'right_arm_elbow_flex', bus:'bus2'};
  const context = {binding, joint:{mode:'position', motor_id:3, position_rad:0}, session:{}, fresh:true,
    bus:{id:'bus2', stamp_ns:'1', robot_id:'r', hardware_id:'h', layout_id:'1'}};
  const calls = [], maps = [];
  const maintenance = {session:'test', joint:binding.joint, bus:'bus2', phase:'ready', ok:true};
  let drop = false;
  globalThis.fetch = async (url, options) => {
    const body = options.body ? JSON.parse(options.body) : null;
    calls.push({url, method:options.method, body});
    if (drop && body) throw new Error('连接断开');
    return {ok:true, json:async () => url.endsWith('/motor') ? maintenance : {overrides:{}}};
  };
  try {
    const panel = createCalibrationPanel(() => context, value => maps.push(value), feedbackAngle);
    el('model').open = true;
    await panel.load();
    el('angle').valueAsNumber = 10;
    await el('record').fire('click');
    context.bus.stamp_ns = '2';
    context.joint.position_rad = -50*deg;
    el('angle').valueAsNumber = 60;
    await el('record').fire('click');
    assert.equal(maps.at(-1)[binding.joint].sign, -1);
    context.fresh = false;
    panel.refresh();
    assert.deepEqual(maps.at(-1), {});
    assert.equal(el('record').disabled, true);
    assert.equal(el('save').hidden, true);
    el('motor').open = true;
    await el('motor').fire('toggle');
    assert.equal(el('next').hidden, false);
    drop = true;
    await el('next').fire('click');
    assert.match(el('motor-status').textContent, /结果待核对/);
    assert.equal(el('next').hidden, true);
    maintenance.phase = 'centered';
    await el('refresh').fire('click');
    assert.equal(el('next').textContent, '记录第一个端点');
    assert.deepEqual(calls.filter(call => call.method === 'POST').map(call => call.body), [{action:'center', session:'test', bus:'bus2'}]);
    panel.leave();
  } finally {
    globalThis.document = doc;
    globalThis.fetch = originalFetch;
  }
});

test('网页按所选电机标零，最大和最小手摆记录，切换关节不能对旧会话发写入', async () => {
  const elements = new Map();
  const element = id => {
    if (!elements.has(id)) elements.set(id, {listeners:{},
      addEventListener(type, cb) { this.listeners[type] = cb; },
      async fire(type) { this.listeners[type]({detail:1}); await new Promise(resolve => setImmediate(resolve)); }});
    return elements.get(id);
  };
  const doc = globalThis.document, originalWindow = globalThis.window, originalFetch = globalThis.fetch;
  const target = {bus:'bus2', joint:'right_arm_wrist_roll'};
  let selected = target, permit = false, delayRead = false, releaseRead, refreshPosition;
  const originalTimer = globalThis.setTimeout;
  globalThis.setTimeout = (callback, millis, ...args) => {
    if (millis === 500) { refreshPosition = callback; return; }
    return originalTimer(callback, millis, ...args);
  };
  const calls = [];
  const el = id => element('cal-' + id);
  const context = () => ({binding:selected, bus:{id:selected.bus, control:{enabled:true}}, fresh:false});
  let session = null;
  const phases = {center:'centered', sample1:'sampled1', sample2:'sampled2', save:'saved'};
  globalThis.document = {getElementById:element};
  globalThis.window = {confirm:() => permit};
  globalThis.fetch = async (url, options) => {
    const body = options.body && JSON.parse(options.body);
    if (!body) {
      const value = session || {ok:false,phase:'idle',error:'尚未开启'};
      if (delayRead) await new Promise(resolve => { releaseRead = resolve; });
      return {ok:value.ok, json:async () => value};
    }
    calls.push(body);
    if (url.endsWith('/start')) {
      assert.deepEqual(body, {...target, supported:true});
      session = {...target, session:'test', ok:true, phase:'ready'};
    } else {
      session = {...session, phase:phases[body.action]};
      if (body.action === 'sample1') session.sample1 = 3500;
      if (body.action === 'sample2') session.sample2 = 600;
    }
    return {ok:session.ok, json:async () => session};
  };
  try {
    const panel = createCalibrationPanel(context, () => {}, feedbackAngle);
    el('motor').open = true;
    await el('motor').fire('toggle');
    assert.equal(el('start').hidden, false);
    await el('start').fire('click');
    assert.deepEqual(calls, []);
    permit = true;
    await el('start').fire('click');
    assert.equal(el('next').textContent, '标零（写入电机）');
    selected = {bus:'bus1', joint:'head_motor_1'};
    panel.refresh();
    assert.equal(el('next').hidden, true);
    await el('next').fire('click');
    assert.equal(calls.length, 1);
    selected = target;
    panel.refresh();
    await el('next').fire('click');
    assert.equal(session.phase, 'centered');
    delayRead = true;
    refreshPosition();
    await el('next').fire('click');
    assert.deepEqual(calls.at(-1), {action:'sample1', bus:target.bus, session:'test'});
    delayRead = false;
    releaseRead();
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(el('next').textContent, '记录另一个端点');
    await el('next').fire('click');
    assert.deepEqual(calls.at(-1), {action:'sample2', bus:target.bus, session:'test'});
    assert.match(el('motor-readings').textContent, /限位范围 600 至 3500 raw/);
    await el('next').fire('click');
    assert.equal(session.phase, 'saved');
    session.phase = 'centered';
    await el('refresh').fire('click');
    const writesBefore = calls.length;
    session = {...session, ok:true, phase:'recovery', position_warning:'检测到跨越编码器边界，请恢复并重选中位'};
    refreshPosition();
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(calls.length, writesBefore);
    assert.match(el('motor-status').textContent, /检测到跨越编码器边界/);
    assert.match(el('motor-status').textContent, /请恢复并重选中位/);
    assert.equal(el('next').textContent, '恢复原电机校准');
    panel.leave();
    // 重连后的首次 GET 只剩交接记录，校准保存和服务归还失败需同时展示。
    session = {...target, ok:true, phase:'failed', error:'恢复保护待机失败', calibration_result:'saved'};
    el('motor').open = true;
    await el('motor').fire('toggle');
    assert.match(el('motor-status').textContent, /电机校准已保存/);
    assert.match(el('motor-status').textContent, /恢复保护待机失败/);
    assert.equal(calls.length, writesBefore);
    panel.leave();
  } finally {
    globalThis.document = doc;
    globalThis.setTimeout = originalTimer;
    globalThis.window = originalWindow;
    globalThis.fetch = originalFetch;
  }
});
