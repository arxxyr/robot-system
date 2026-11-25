// URDF 变换为行主序 3×4 仿射矩阵，最后一行恒为 [0, 0, 0, 1]。
function origin([x, y, z], [roll, pitch, yaw]) {
  const a = Math.cos(roll), b = Math.sin(roll);
  const c = Math.cos(pitch), d = Math.sin(pitch);
  const e = Math.cos(yaw), f = Math.sin(yaw);
  return [
    e*c, e*d*b-f*a, e*d*a+f*b, x,
    f*c, f*d*b+e*a, f*d*a-e*b, y,
    -d, c*b, c*a, z,
  ];
}

function rotation([x, y, z], angle) {
  const c = Math.cos(angle), s = Math.sin(angle), t = 1-c;
  return [
    t*x*x+c, t*x*y-s*z, t*x*z+s*y, 0,
    t*x*y+s*z, t*y*y+c, t*y*z-s*x, 0,
    t*x*z-s*y, t*y*z+s*x, t*z*z+c, 0,
  ];
}

function compose(a, b) {
  const result = [];
  for (let row = 0; row < 12; row += 4) {
    for (let col = 0; col < 4; col++) {
      result[row+col] = a[row]*b[col] + a[row+1]*b[4+col] + a[row+2]*b[8+col];
    }
    result[row+3] += a[row+3];
  }
  return result;
}

function point(m, [x, y, z]) {
  return [m[0]*x+m[1]*y+m[2]*z+m[3], m[4]*x+m[5]*y+m[6]*z+m[7], m[8]*x+m[9]*y+m[10]*z+m[11]];
}

// 相邻顶点按从外侧观察的逆时针顺序排列，底盖和顶盖共用同一圈侧面顶点。
function solid(shape) {
  let ring, height;
  if (shape.kind === 'box') {
    const [width, depth, length] = shape.size;
    ring = [[-width/2, -depth/2], [width/2, -depth/2], [width/2, depth/2], [-width/2, depth/2]];
    height = length;
  } else if (shape.kind === 'cylinder') {
    const [radius, length] = shape.size;
    ring = Array.from({length: 16}, (_, i) => {
      const angle = i * Math.PI / 8;
      return [radius*Math.cos(angle), radius*Math.sin(angle)];
    });
    height = length;
  } else {
    throw new Error('姿态模型几何类型无效');
  }
  const local = origin(shape.xyz, shape.rpy);
  const vertices = [-height/2, height/2].flatMap(z => ring.map(([x, y]) => point(local, [x, y, z])));
  const n = ring.length;
  const top = ring.map((_, i) => n+i);
  const bottom = ring.map((_, i) => n-1-i);
  const sides = ring.map((_, i) => [i, (i+1)%n, (i+1)%n+n, i+n]);
  return {vertices, faces: [bottom, top, ...sides]};
}

function rgb(hex) {
  const value = Number.parseInt(hex.slice(1), 16);
  return [value >> 16 & 255, value >> 8 & 255, value & 255];
}

function tint(color, light) {
  return 'rgb(' + color.map(channel => Math.round(channel * light)).join(',') + ')';
}

// 夹爪电机行程按现有校准映射至 URDF 张角。控制面板关闭时可用快照的 raw 范围。
function feedbackAngle(node, bus, feedback) {
  if (feedback?.mode !== 'position' || !Number.isFinite(feedback.position_rad)) return null;
  if (!node.range) return (node.sign ?? 1)*feedback.position_rad + (node.offset ?? 0);
  const limit = bus.control?.joints?.find(joint => joint.name === node.joint);
  let span = [limit?.min_position_rad, limit?.max_position_rad];
  let value = feedback.position_rad;
  if (!span.every(Number.isFinite)) {
    span = feedback.target_range_raw;
    value = feedback.position_raw;
  }
  if (!span || !span.every(Number.isFinite) || span[1] <= span[0] || !Number.isFinite(value)) return null;
  const [closed, open] = node.range;
  return closed + (open-closed)*(value-span[0])/(span[1]-span[0]);
}

const neutral = [164, 176, 185], selectedColor = [247, 163, 54];

export function createPoseView(canvas, model, onSelect) {
  const context = canvas.getContext('2d');
  if (!context) throw new Error('浏览器无法绘制姿态图');
  const byName = new Map();
  const moving = [];
  const nodes = model.nodes.map(node => {
    const parent = byName.get(node.parent);
    const entry = {
      ...node, parent, local: origin(node.xyz, node.rpy), owner: parent?.owner,
      angle: node.offset ?? 0, stamp: null, fresh: false,
    };
    if (node.joint) {
      entry.owner = entry;
      moving.push(entry);
    }
    byName.set(node.name, entry);
    return entry;
  });
  const solids = model.shapes.map(shape => ({
    ...solid(shape), node: byName.get(shape.node), color: rgb(shape.color),
  }));

  const defaultAzimuth = -2.32, defaultElevation = 0.21;
  let azimuth = defaultAzimuth, elevation = defaultElevation;
  let zoom = 1, panX = 0, panY = 0, panMode = false;
  let visible = false, frame = 0, selected = null;
  const pointers = new Map();
  let pinch = null;
  let markers = [], surfaces = [];

  function redraw() {
    if (visible && !document.hidden && !frame) frame = requestAnimationFrame(draw);
  }

  function draw() {
    frame = 0;
    const {width, height} = canvas.getBoundingClientRect();
    if (!width || !height) return;
    const ratio = Math.min(devicePixelRatio || 1, 2);
    const pixelsX = Math.round(width*ratio), pixelsY = Math.round(height*ratio);
    if (canvas.width !== pixelsX || canvas.height !== pixelsY) {
      canvas.width = pixelsX;
      canvas.height = pixelsY;
    }
    context.setTransform(pixelsX/width, 0, 0, pixelsY/height, 0, 0);
    context.clearRect(0, 0, width, height);
    const scale = zoom*Math.min(width/1.16, height/1.48);
    const ca = Math.cos(azimuth), sa = Math.sin(azimuth);
    const ce = Math.cos(elevation), se = Math.sin(elevation);
    function project([x, y, z]) {
      const forward = ca*x + sa*y;
      const vertical = z - 0.59;
      return [
        width/2 + panX + scale*(-sa*x + ca*y),
        height/2 + panY - scale*(ce*vertical - se*forward),
        ce*forward + se*vertical,
      ];
    }
    function path(points) {
      context.beginPath();
      context.moveTo(points[0][0], points[0][1]);
      for (const [x, y] of points.slice(1)) context.lineTo(x, y);
    }
    context.lineWidth = 1;
    context.strokeStyle = '#e3eaf0';
    for (let i = -3; i <= 3; i++) {
      const at = i*0.16;
      path([project([at,-0.48,0]), project([at,0.48,0])]);
      context.stroke();
      path([project([-0.48,at,0]), project([0.48,at,0])]);
      context.stroke();
    }
    for (const node of nodes) {
      let world = node.parent ? compose(node.parent.world, node.local) : node.local;
      if (node.joint) world = compose(world, rotation(node.axis, node.angle));
      node.world = world;
    }
    const faces = [];
    for (const shape of solids) {
      const owner = shape.node.owner;
      const active = owner && owner.joint === selected;
      const color = active && owner.fresh ? selectedColor : owner && !owner.fresh ? neutral : shape.color;
      const world = shape.vertices.map(vertex => point(shape.node.world, vertex));
      const projected = world.map(project);
      for (const indices of shape.faces) {
        const poly = indices.map(i => projected[i]);
        const [a, b, c] = poly;
        if ((b[0]-a[0])*(c[1]-a[1]) - (b[1]-a[1])*(c[0]-a[0]) >= 0) continue;
        const [u, v, w] = indices.slice(0, 3).map(i => world[i]);
        const dx = v[0]-u[0], dy = v[1]-u[1], dz = v[2]-u[2];
        const ex = w[0]-u[0], ey = w[1]-u[1], ez = w[2]-u[2];
        const nx = dy*ez-dz*ey, ny = dz*ex-dx*ez, nz = dx*ey-dy*ex;
        const light = 0.82 + 0.18*(-0.36*nx-0.48*ny+0.8*nz)/Math.hypot(nx,ny,nz);
        faces.push({
          poly, owner, fill: tint(color, light),
          depth: poly.reduce((total, p) => total + p[2], 0)/poly.length,
        });
      }
    }
    surfaces = faces.sort((a, b) => a.depth - b.depth);
    context.lineWidth = 0.55;
    context.lineJoin = 'round';
    for (const face of surfaces) {
      path(face.poly);
      context.closePath();
      context.fillStyle = context.strokeStyle = face.fill;
      context.fill();
      context.stroke();
    }
    markers = moving.map(node => ({
      node, p: project([node.world[3], node.world[7], node.world[11]]),
    })).sort((a,b) => Number(a.node.joint === selected)-Number(b.node.joint === selected) || a.p[2]-b.p[2]);
    for (const {node, p: [x, y]} of markers) {
      const active = node.joint === selected;
      context.beginPath();
      context.arc(x, y, active ? 7 : 3.5, 0, 2*Math.PI);
      context.fillStyle = active ? '#a45808' : node.fresh ? '#224967' : '#8997a2';
      context.fill();
      context.strokeStyle = '#ffffff';
      context.lineWidth = active ? 2 : 1;
      context.stroke();
    }
  }

  function update(buses) {
    // 即使最新采集失败，服务端仍可能带回一帧经过校验的位置和源时间戳。
    const stamps = new Map(buses.map(bus => [bus.id,
      typeof bus.stamp_ns === 'string' && /^[1-9]\d{0,19}$/.test(bus.stamp_ns)
        ? BigInt(bus.stamp_ns) : null,
    ]));
    let changed = false;
    for (const node of moving) {
      const bus = buses.find(bus => bus.id === node.bus);
      const stamp = stamps.get(node.bus);
      const feedback = bus?.joints?.find(joint => joint.name === node.joint);
      const angle = feedbackAngle(node, bus, feedback);
      const valid = stamp != null && Number.isFinite(angle);
      const fresh = bus?.data_status === 'fresh' && valid;
      if (fresh || (valid && (node.stamp === null || stamp > node.stamp))) {
        changed ||= node.stamp === null || node.angle !== angle;
        node.angle = angle;
        node.stamp = stamp;
      }
      changed ||= fresh !== node.fresh;
      node.fresh = fresh;
    }
    if (changed) redraw();
  }

  function pick(x, y) {
    let nearest, distance = 12;
    for (const {node, p} of markers) {
      const d = Math.hypot(p[0]-x, p[1]-y);
      if (d <= distance) { nearest = node.joint; distance = d; }
    }
    if (nearest) return onSelect(nearest);
    // 与绘制相反的顺序命中可见面。固定底座也参与遮挡。
    for (let i = surfaces.length-1; i >= 0; i--) {
      const {poly, owner} = surfaces[i];
      const inside = poly.every(([ax, ay], index) => {
        const [bx, by] = poly[(index+1) % poly.length];
        return (bx-ax)*(y-ay) - (by-ay)*(x-ax) <= 0;
      });
      if (inside) {
        if (owner) onSelect(owner.joint);
        return;
      }
    }
  }
  // 锚点使用以画布中心为原点的 CSS 像素，缩放后仍指向同一模型位置。
  function boundZoom(value) { return Math.max(0.5, Math.min(4, value)); }
  function zoomAt(factor, x, y) {
    if (!Number.isFinite(factor) || factor <= 0) return;
    const next = boundZoom(zoom*factor);
    if (next === zoom) return;
    const ratio = next/zoom;
    panX = x + (panX-x)*ratio;
    panY = y + (panY-y)*ratio;
    zoom = next;
    redraw();
  }
  function clearPointers() {
    for (const id of pointers.keys()) {
      if (canvas.hasPointerCapture(id)) canvas.releasePointerCapture(id);
    }
    pointers.clear();
    pinch = null;
  }
  function setPanMode(value) {
    clearPointers();
    panMode = value;
    canvas.style.cursor = panMode ? 'move' : 'grab';
  }
  function resetView() {
    setPanMode(false);
    azimuth = defaultAzimuth;
    elevation = defaultElevation;
    zoom = 1;
    panX = panY = 0;
    redraw();
  }

  canvas.style.touchAction = 'none';
  setPanMode(false);
  canvas.addEventListener('wheel', e => {
    if (!e.deltaY) return;
    e.preventDefault();
    const rect = canvas.getBoundingClientRect();
    const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? rect.height : 1;
    const factor = Math.exp(Math.max(-3, Math.min(3, -e.deltaY*unit*0.002)));
    zoomAt(factor, e.clientX-rect.left-rect.width/2, e.clientY-rect.top-rect.height/2);
  }, {passive: false});

  const arrowPan = {ArrowLeft: [-32, 0], ArrowRight: [32, 0], ArrowUp: [0, -32], ArrowDown: [0, 32]};
  canvas.addEventListener('keydown', e => {
    if (e.altKey || e.ctrlKey || e.metaKey || e.shiftKey || !Object.hasOwn(arrowPan, e.key)) return;
    e.preventDefault();
    const [dx, dy] = arrowPan[e.key];
    panX += dx;
    panY += dy;
    redraw();
  });
  function touchPair() {
    const [a, b] = pointers.values();
    const rect = canvas.getBoundingClientRect();
    return {
      x: (a.x+b.x)/2-rect.left-rect.width/2,
      y: (a.y+b.y)/2-rect.top-rect.height/2,
      distance: Math.hypot(a.x-b.x, a.y-b.y),
    };
  }
  canvas.addEventListener('pointerdown', e => {
    if (e.button !== 0 || pointers.size >= 2) return;
    const [first] = pointers.values();
    if (first && (first.type !== 'touch' || e.pointerType !== 'touch')) return;
    pointers.set(e.pointerId, {
      type: e.pointerType, x: e.clientX, y: e.clientY,
      startX: e.clientX, startY: e.clientY, dragged: Boolean(first),
    });
    if (first) {
      first.dragged = true;
      pinch = {...touchPair(), panX, panY, zoom};
    }
    canvas.setPointerCapture(e.pointerId);
  });
  canvas.addEventListener('pointermove', e => {
    const pointer = pointers.get(e.pointerId);
    if (!pointer) return;
    const {x, y} = pointer;
    pointer.x = e.clientX;
    pointer.y = e.clientY;
    let dx = pointer.x-x, dy = pointer.y-y;
    if (pointers.size === 2) {
      // 以双指落下时的两点为基准，触及缩放上下限后平移也不会积累误差。
      const pair = touchPair();
      zoom = pinch.distance >= 1 ? boundZoom(pinch.zoom*pair.distance/pinch.distance) : pinch.zoom;
      panX = pair.x - (pinch.x-pinch.panX)*zoom/pinch.zoom;
      panY = pair.y - (pinch.y-pinch.panY)*zoom/pinch.zoom;
      if (pinch.distance < 1 && pair.distance >= 1) pinch = {...pair, panX, panY, zoom};
    } else {
      if (!pointer.dragged) {
        dx = pointer.x-pointer.startX;
        dy = pointer.y-pointer.startY;
        if (Math.hypot(dx, dy) < 4) return;
        pointer.dragged = true;
      }
      if (panMode) {
        panX += dx;
        panY += dy;
      } else {
        azimuth -= dx*0.009;
        elevation = Math.max(-0.25, Math.min(1.2, elevation+dy*0.009));
      }
    }
    redraw();
  });
  canvas.addEventListener('pointerup', e => {
    const pointer = pointers.get(e.pointerId);
    pointers.delete(e.pointerId);
    if (pointer && !pointer.dragged) {
      const rect = canvas.getBoundingClientRect();
      pick(e.clientX-rect.left, e.clientY-rect.top);
    }
  });
  const endPointer = e => pointers.delete(e.pointerId);
  canvas.addEventListener('pointercancel', endPointer);
  canvas.addEventListener('lostpointercapture', endPointer);
  const size = new ResizeObserver(redraw);
  size.observe(canvas);
  document.addEventListener('visibilitychange', redraw);
  return {
    update, setPanMode, resetView,
    zoomBy(factor) { zoomAt(factor, 0, 0); },
    select(jointName) { if (selected !== jointName) { selected = jointName; redraw(); } },
    setVisible(value) {
      visible = value;
      if (visible) redraw();
      else {
        clearPointers();
        if (frame) { cancelAnimationFrame(frame); frame = 0; }
      }
    },
  };
}
