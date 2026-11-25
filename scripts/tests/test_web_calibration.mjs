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
    assert.equal(el('next').textContent, '记录第一端');
    assert.deepEqual(calls.filter(call => call.method === 'POST').map(call => call.body), [{action:'center', session:'test', bus:'bus2'}]);
  } finally {
    globalThis.document = doc;
    globalThis.fetch = originalFetch;
  }
});
