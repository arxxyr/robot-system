import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {test} from 'node:test';

const source = await readFile(new URL('../../crates/robot-rpc/src/status_pose.js', import.meta.url));
const {initArmPreviews} = await import('data:text/javascript;base64,' + source.toString('base64'));

function mockPanel(t) {
  const elements = new Map();
  function element(id) {
    if (!elements.has(id)) elements.set(id, {
      addEventListener(event, callback) { this[event] = callback; },
      replaceChildren(...rows) { this.rows = rows; },
    });
    return elements.get(id);
  }
  const doc = globalThis.document, originalFetch = globalThis.fetch;
  t.after(() => { globalThis.document = doc; globalThis.fetch = originalFetch; });
  globalThis.document = {getElementById:element, createElement() {
    return {
      cells: [],
      append(cell) { this.cells.push(cell); },
      insertCell() { const cell = {}; this.append(cell); return cell; },
    };
  }};
  element('ik-side').value = 'right';
  return element;
}

test('右腕预览逐次取帧、保留已算出的段，并展示反馈过期原因', async t => {
  const element = mockPanel(t);
  const start = [1900, 2100, 2900, 1850, 2050];
  const plan = {stamp_ns:'1024', joint_names:['肩旋转','肩抬升','肘','腕俯仰','腕旋转'],
    start:{xyz_mm:[37.12, 0, 122.35], joints_raw:[...start]}, target_xyz_mm:[47.12, 0, 122.35],
    forward:{joints_raw:[1900, 2103, 2890, 1857, 2050], error:null}, return:{joints_raw:[...start], error:null}};
  const requests = [];
  let reply = {ok:true, json:async () => structuredClone(plan)};
  globalThis.fetch = async (url, options) => { requests.push([url, options.method ?? 'GET']); return reply; };

  initArmPreviews(name => name);
  const run = element('ik-run'), rows = element('ik-joints');
  assert.equal(requests.length, 0);
  await Promise.all([run.click(), run.click()]);
  assert.deepEqual(requests, [['/api/kinematics/right-arm/preview','GET']]);
  assert.match(element('ik-points').textContent, /\(37, 0, 122\).*\(47, 0, 122\)/);
  assert.equal(element('ik-return').textContent, '通过');
  plan.stamp_ns = '2048';
  plan.start.joints_raw[2] = 2997;
  plan.return = {joints_raw:null, error:'右肘目标 2997 raw 超过主动上限 2991 raw'};
  assert.match(element('ik-status').textContent, /1024/);
  assert.equal(rows.rows[2].cells[1].textContent, 2900);
  await run.click();
  assert.match(element('ik-status').textContent, /2048/);
  assert.equal(element('ik-forward').textContent, '通过');
  assert.equal(element('ik-return').textContent, plan.return.error);
  assert.equal(rows.rows[2].cells[3].textContent, '—');

  plan.forward = {joints_raw:null, error:'前移不可达'};
  plan.return = {joints_raw:null, error:null};
  await run.click();
  assert.equal(element('ik-forward').textContent, '前移不可达');
  assert.match(element('ik-return').textContent, /未计算/);

  reply = {ok:false, json:async () => ({error:'右臂反馈已过期'})};
  await run.click();
  assert.equal(element('ik-result').hidden, true);
  assert.match(element('ik-status').textContent, /右臂反馈已过期/);
  assert.equal(run.disabled, false);
});

test('切换到左臂只在点击时读 bus1，丢弃切侧和断线前的返回', async t => {
  const element = mockPanel(t);
  const requests = [];
  globalThis.fetch = (url, options) => new Promise(resolve => {
    requests.push({url, method: options.method ?? 'GET', signal: options.signal, resolve});
  });
  const reset = initArmPreviews(name => name);
  const run = element('ik-run'), side = element('ik-side'), result = element('ik-result');
  const right = run.click();
  side.value = 'left';
  side.change();
  assert.equal(requests.length, 1);
  assert.ok(requests[0].signal.aborted);
  const left = run.click();
  assert.deepEqual(requests.map(({url, method}) => [url, method]), [
    ['/api/kinematics/right-arm/preview', 'GET'], ['/api/kinematics/left-arm/preview', 'GET'],
  ]);
  // 模拟旧请求在取消之后仍返回，不得唤醒或覆盖新的预览。
  requests[0].resolve({ok:false, json:async () => ({error:'右臂反馈过期'})});
  await right;
  assert.ok(run.disabled);
  assert.ok(result.hidden);
  assert.match(element('ik-status').textContent, /左臂/);
  const joints = [1900, 2100, 2450, 2000, 2050];
  const leg = {joints_raw: [...joints], error: null};
  requests[1].resolve({ok:true, json:async () => ({
    stamp_ns:'3072', joint_names:['肩旋转','肩抬升','肘','腕俯仰','腕旋转'],
    start:{xyz_mm:[80, -15, 120], joints_raw:joints}, target_xyz_mm:[90, -15, 120], forward:leg, return:leg,
  })});
  await left;
  assert.equal(result.hidden, false);
  assert.match(element('ik-points').textContent, /左肩基座.*\(80, -15, 120\).*\(90, -15, 120\)/);
  assert.match(element('ik-status').textContent, /bus1.*3072/);
  assert.equal(element('ik-joints').rows.length, 5);

  const next = run.click();
  reset();
  assert.ok(requests[2].signal.aborted);
  requests[2].resolve({ok:false, json:async () => ({error:'旧连接'})});
  await next;
  assert.ok(result.hidden);
  assert.equal(element('ik-status').textContent, '');
  assert.equal(run.disabled, false);
  assert.equal(requests.length, 3);
});

test('回位对照每次点击只读所选侧一帧，保留符号和逐轴容差，独立于腕点预览', async t => {
  const element = mockPanel(t);
  const names = ['肩部旋转', '肩部抬升', '肘关节', '腕部俯仰', '腕部旋转'];
  const joints = [0, 20, -20, 21, -21].map((error, i) => ({
    name: names[i], position_raw: 2000 + error, target_raw: 2000, error_raw: error,
    within_tolerance: Math.abs(error) <= 20,
  }));
  const frame = {stamp_ns: '12345678901234567890', joints};
  const requests = [];
  globalThis.fetch = (url, options) => new Promise(resolve => {
    requests.push({url, method: options.method ?? 'GET', signal: options.signal, resolve});
  });
  const reset = initArmPreviews(name => name);
  const run = element('home-reference-run'), side = element('ik-side');
  const status = element('home-reference-status'), result = element('home-reference-result');
  assert.equal(requests.length, 0);
  const right = run.click();
  await run.click();
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, '/api/kinematics/right-arm/home-reference');
  assert.equal(requests[0].method, 'GET');
  requests[0].resolve({ok: true, json: async () => frame});
  await right;
  assert.equal(result.hidden, false);
  assert.match(status.textContent, /右臂.*bus2.*12345678901234567890.*仅本帧 3\/5/);
  const rows = element('home-reference-joints').rows;
  assert.deepEqual(rows.map(row => row.cells.map(cell => cell.textContent)), [
    [names[0], 2000, 2000, 0, '容差内'],
    [names[1], 2020, 2000, '+20', '容差内'],
    [names[2], 1980, 2000, -20, '容差内'],
    [names[3], 2021, 2000, '+21', '容差外'],
    [names[4], 1979, 2000, -21, '容差外'],
  ]);
  frame.stamp_ns = '222';
  joints[1].position_raw = 2100;
  const ik = element('ik-run').click();
  requests[1].resolve({ok: false, json: async () => ({error: '无法预览腕点'})});
  await ik;
  assert.match(status.textContent, /12345678901234567890/);
  assert.equal(rows[1].cells[1].textContent, 2020);
  assert.equal(result.hidden, false);

  const oldRight = run.click();
  side.value = 'left';
  side.change();
  assert.ok(requests[2].signal.aborted);
  assert.ok(result.hidden);
  assert.equal(status.textContent, '');
  const left = run.click();
  assert.equal(requests[3].url, '/api/kinematics/left-arm/home-reference');
  assert.equal(requests[3].method, 'GET');
  requests[2].resolve({ok: true, json: async () => frame});
  await oldRight;
  assert.ok(result.hidden);
  assert.ok(run.disabled);
  assert.match(status.textContent, /左臂/);
  requests[3].resolve({ok: false, json: async () => ({error: '左臂反馈已过期'})});
  await left;
  assert.ok(result.hidden);
  assert.match(status.textContent, /左臂反馈已过期/);
  assert.equal(run.disabled, false);

  const oldLeft = run.click();
  reset();
  assert.ok(requests[4].signal.aborted);
  requests[4].resolve({ok: true, json: async () => frame});
  await oldLeft;
  assert.ok(result.hidden);
  assert.equal(status.textContent, '');
});
