// 可视化使用随状态页发布的 XLeRobot URDF。模型树、视觉几何和安装位姿都取自原文件。
const urdfUrl = '/assets/xlerobot/xlerobot.urdf';

function vector(value, fallback) {
  if (value === null || value === undefined) return fallback;
  const fields = value.trim().split(/\s+/).map(Number);
  if (fields.length !== fallback.length || fields.some(v => !Number.isFinite(v))) {
    throw new Error('URDF 向量格式无效');
  }
  return fields;
}

function placement(element) {
  const origin = element.querySelector(':scope > origin');
  return {
    xyz: vector(origin?.getAttribute('xyz'), [0, 0, 0]),
    rpy: vector(origin?.getAttribute('rpy'), [0, 0, 0]),
  };
}

export function parseUrdf(xml, bindings) {
  const root = new DOMParser().parseFromString(xml, 'application/xml').documentElement;
  if (root.tagName !== 'robot') throw new Error('URDF XML 无效');
  const materials = new Map(Array.from(root.querySelectorAll(':scope > material'), material =>
    [material.getAttribute('name'), vector(material.querySelector('color')?.getAttribute('rgba'), [0.7, 0.7, 0.7, 1])]));
  const links = new Map();
  const visuals = [];
  for (const link of root.querySelectorAll(':scope > link')) {
    const name = link.getAttribute('name');
    const node = {name, children: [], xyz: [0, 0, 0], rpy: [0, 0, 0]};
    if (!name || links.has(name)) throw new Error('URDF 连杆名称缺失或重复');
    links.set(name, node);
    for (const visual of link.querySelectorAll(':scope > visual')) {
      const mesh = visual.querySelector(':scope > geometry > mesh');
      const path = mesh?.getAttribute('filename');
      if (!/^meshes\/[\w.-]+\.stl$/i.test(path ?? '')) throw new Error('未找到本地 URDF 视觉网格：' + name);
      const material = visual.querySelector(':scope > material');
      const color = vector(material?.querySelector('color')?.getAttribute('rgba'),
        materials.get(material?.getAttribute('name')) ?? [0.7, 0.7, 0.7, 1]);
      visuals.push({node, path, color, scale: vector(mesh.getAttribute('scale'), [1, 1, 1]), ...placement(visual)});
    }
  }
  const mapping = new Map(bindings.map(n => [n.urdfJoint, n]));
  for (const j of root.querySelectorAll(':scope > joint')) {
    const node = links.get(j.querySelector('child')?.getAttribute('link'));
    const parent = links.get(j.querySelector('parent')?.getAttribute('link'));
    if (!parent || !node || node.parent) throw new Error('URDF 关节连接无效');
    parent.children.push(node);
    node.parent = parent;
    Object.assign(node, placement(j));
    const binding = mapping.get(j.getAttribute('name'));
    if (binding) {
      if (!['revolute','continuous'].includes(j.getAttribute('type'))) throw new Error('反馈关节类型不匹配');
      const axis = vector(j.querySelector('axis')?.getAttribute('xyz'), [1, 0, 0]);
      const length = Math.hypot(...axis);
      if (!length) throw new Error('关节轴长度为零');
      Object.assign(node, binding, {axis: axis.map(v => v / length)});
      mapping.delete(binding.urdfJoint);
    }
  }
  const nodes = [];
  const visit = node => { nodes.push(node); node.children.forEach(visit); };
  const roots = [...links.values()].filter(node => !node.parent);
  if (roots.length === 1) visit(roots[0]);
  if (!nodes.length || nodes.length !== links.size || mapping.size) throw new Error('URDF 模型树与电机映射不完整');
  return {nodes, visuals};
}

// 原模型使用小端二进制 STL。每个文件只解码一次，左右臂共用顶点和法线。
export function decodeStl(bytes) {
  const data = new DataView(bytes);
  const triangles = bytes.byteLength >= 84 ? data.getUint32(80, true) : 0;
  if (!triangles || bytes.byteLength !== 84 + triangles * 50) throw new Error('STL 三角面数据不完整');
  const result = new Float32Array(triangles * 18);
  let output = 0;
  for (let face = 84; face < bytes.byteLength; face += 50) {
    const nx = data.getFloat32(face, true), ny = data.getFloat32(face+4, true), nz = data.getFloat32(face+8, true);
    for (let vertex = face+12; vertex < face+48; vertex += 12) {
      result[output++] = data.getFloat32(vertex, true);
      result[output++] = data.getFloat32(vertex+4, true);
      result[output++] = data.getFloat32(vertex+8, true);
      result[output++] = nx; result[output++] = ny; result[output++] = nz;
    }
  }
  return result;
}

export async function loadUrdf(bindings) {
  const response = await fetch(urdfUrl);
  if (!response.ok) throw new Error('无法读取 XLeRobot URDF');
  const model = parseUrdf(await response.text(), bindings);
  const paths = [...new Set(model.visuals.map(item => item.path))];
  model.meshes = new Map(await Promise.all(paths.map(async path => {
    const mesh = await fetch(new URL(path, response.url));
    if (!mesh.ok) throw new Error('无法读取 ' + path);
    return [path, decodeStl(await mesh.arrayBuffer())];
  })));
  return model;
}

const vert = `#version 300 es
in vec3 position;
in vec3 normal;
uniform mat4 world;
uniform mat4 camera;
uniform vec3 color;
uniform int mode;
uniform float diameter;
out vec3 lit;
void main() {
  gl_Position = camera * world * vec4(position, 1.0);
  gl_PointSize = diameter;
  lit = color;
  if (mode == 0) {
    vec3 n = normalize(transpose(inverse(mat3(world))) * normal);
    lit *= 0.67 + 0.33 * max(dot(n, normalize(vec3(0.35, -0.48, 0.8))), 0.0);
  }
}`;
const frag = `#version 300 es
precision highp float;
precision highp int;
in vec3 lit;
uniform int mode;
out vec4 result;
void main() {
  if (mode == 2 && distance(gl_PointCoord, vec2(0.5)) > 0.5) discard;
  result = vec4(lit, 1.0);
}`;

function program(gl) {
  const result = gl.createProgram();
  for (const [type, source] of [[gl.VERTEX_SHADER, vert], [gl.FRAGMENT_SHADER, frag]]) {
    const shader = gl.createShader(type);
    gl.shaderSource(shader, source); gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      const error = gl.getShaderInfoLog(shader);
      gl.deleteShader(shader);
      gl.deleteProgram(result);
      throw new Error('模型着色器编译失败：' + error);
    }
    gl.attachShader(result, shader);
    gl.deleteShader(shader);
  }
  gl.linkProgram(result);
  if (!gl.getProgramParameter(result, gl.LINK_STATUS)) {
    const error = gl.getProgramInfoLog(result);
    gl.deleteProgram(result);
    throw new Error('模型着色程序链接失败：' + error);
  }
  return result;
}

// 所有视觉面常驻客户端显存。反馈更新只传变换矩阵，点击时才绘制一个像素的拾取缓冲。
export function meshRenderer(canvas, model, redraw, reportError) {
  const gl = canvas.getContext('webgl2', {alpha:true, antialias:true});
  if (!gl) throw new Error('浏览器未启用 WebGL 2');
  let gpu, uniforms, meshes, pixelTarget;
  const identity = new Float32Array([1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1]);
  const matrix = new Float32Array(identity), pixel = new Uint8Array(4);
  function load() {
    gpu = program(gl);
    gl.useProgram(gpu);
    uniforms = Object.fromEntries(['world','camera','color','mode','diameter']
      .map(n => [n, gl.getUniformLocation(gpu, n)]));
    const position = gl.getAttribLocation(gpu, 'position'), normal = gl.getAttribLocation(gpu, 'normal');
    function buffer(data) {
      const array = gl.createVertexArray();
      gl.bindVertexArray(array);
      gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
      gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
      for (const [index, offset] of [[position,0], [normal,12]]) {
        gl.enableVertexAttribArray(index);
        gl.vertexAttribPointer(index, 3, gl.FLOAT, false, 24, offset);
      }
      return {array, count:data.length/6};
    }
    meshes = new Map([...model.meshes].map(([name, data]) => [name, buffer(data)]));
    const grid = [];
    for (let i=-3; i<=3; i++) {
      const at=i*0.16;
      for (const [x,y] of [[at,-0.48],[at,0.48],[-0.48,at],[0.48,at]]) grid.push(x,y,0,0,0,1);
    }
    meshes.set('grid',buffer(new Float32Array(grid)));
    meshes.set('joint',buffer(new Float32Array([0,0,0,0,0,1])));
    pixelTarget = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, pixelTarget);
    for (const [format, attachment] of [[gl.RGBA8,gl.COLOR_ATTACHMENT0], [gl.DEPTH_COMPONENT16,gl.DEPTH_ATTACHMENT]]) {
      const renderbuffer = gl.createRenderbuffer();
      gl.bindRenderbuffer(gl.RENDERBUFFER,renderbuffer);
      gl.renderbufferStorage(gl.RENDERBUFFER,format,1,1);
      gl.framebufferRenderbuffer(gl.FRAMEBUFFER,attachment,gl.RENDERBUFFER,renderbuffer);
    }
    if (gl.checkFramebufferStatus(gl.FRAMEBUFFER)!==gl.FRAMEBUFFER_COMPLETE) throw new Error('模型拾取缓冲不可用');
    gl.bindFramebuffer(gl.FRAMEBUFFER,null);
    gl.disable(gl.DITHER);
  }
  load();
  canvas.addEventListener('webglcontextlost', e => {e.preventDefault();reportError('图形上下文已暂停，正在等待恢复。');});
  canvas.addEventListener('webglcontextrestored', () => {
    try {load();reportError('');redraw();}
    catch (e) {reportError(e.message);}
  });
  function object(geometry, kind, transform, color, mode) {
    gl.uniformMatrix4fv(uniforms.world,false,transform);
    gl.uniform3fv(uniforms.color,color);
    gl.uniform1i(uniforms.mode,mode);
    gl.bindVertexArray(geometry.array);
    gl.drawArrays(kind,0,geometry.count);
  }
  // URDF 运算为行主序 3×4，WebGL uniform 使用列主序 4×4。
  function toGL(m, scale) {
    for (let row=0;row<3;row++) {
      for (let col=0;col<3;col++) matrix[col*4+row]=m[row*4+col]*(scale?.[col] ?? 1);
      matrix[12+row]=m[4*row+3];
    }
    return matrix;
  }
  return (camera, moving, selected, ratio, hit) => {
    if (gl.isContextLost()) return;
    const w=canvas.width,h=canvas.height;
    gl.bindFramebuffer(gl.FRAMEBUFFER,hit ? pixelTarget : null);
    gl.viewport(hit ? -Math.floor(hit[0]*ratio) : 0,hit ? -(h-1-Math.floor(hit[1]*ratio)) : 0,w,h);
    gl.clearColor(0,0,0,0);
    gl.clear(gl.COLOR_BUFFER_BIT|gl.DEPTH_BUFFER_BIT);
    gl.enable(gl.DEPTH_TEST);
    gl.useProgram(gpu);
    gl.uniformMatrix4fv(uniforms.camera,false,camera);
    if (!hit) object(meshes.get('grid'),gl.LINES,identity,[0.87,0.9,0.92],1);
    for (const visual of model.visuals) {
      const owner=visual.node.owner;
      let color=visual.color.slice(0,3);
      if (hit) color=[(owner?.pickId ?? 0)/255,0,0];
      else if (owner?.joint===selected && owner.fresh) color=[0.98,0.62,0.16];
      else if (owner && !owner.fresh) color=color.map(c=>0.43+0.32*c);
      object(meshes.get(visual.path),gl.TRIANGLES,toGL(visual.world,visual.scale),color,hit ? 1 : 0);
    }
    gl.disable(gl.DEPTH_TEST);
    // 关节中心即使与网格重叠也能选中；选中项最后绘制。
    const markers=moving.slice().sort((a,b)=>Number(a.joint===selected)-Number(b.joint===selected));
    for (const node of markers) {
      const active=node.joint===selected;
      gl.uniform1f(uniforms.diameter,(active ? 12 : 7)*ratio);
      const color=hit ? [node.pickId/255,0,0] : active ? [0.71,0.34,0.02] : node.fresh ? [0.12,0.32,0.44] : [0.48,0.55,0.58];
      object(meshes.get('joint'),gl.POINTS,toGL(node.world),color,2);
    }
    if (hit) {
      gl.readPixels(0,0,1,1,gl.RGBA,gl.UNSIGNED_BYTE,pixel);
      gl.bindFramebuffer(gl.FRAMEBUFFER,null);
      return moving[pixel[0]-1]?.joint;
    }
  };
}
