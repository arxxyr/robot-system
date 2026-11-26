import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {test} from 'node:test';

const source = await readFile(new URL('../../crates/robot-rpc/src/status_urdf.js', import.meta.url));
const {decodeStl} = await import('data:text/javascript;base64,' + source.toString('base64'));

test('二进制 STL 按小端读取三角顶点和法线，拒绝截断网格', () => {
  const bytes = new ArrayBuffer(134), view = new DataView(bytes);
  view.setUint32(80, 1, true);
  const face = [0,0,1, 1,2,3, 4,5,6, 7,8,9];
  face.forEach((value, i) => view.setFloat32(84 + 4*i, value, true));
  assert.deepEqual([...decodeStl(bytes)], [1,2,3,0,0,1, 4,5,6,0,0,1, 7,8,9,0,0,1]);
  assert.throws(() => decodeStl(bytes.slice(0, 133)), /三角面/);
});
