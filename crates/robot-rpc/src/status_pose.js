// 预览取服务端同一帧的反馈。画布后续收到的姿态只更新实时模型。
export function initWristPreview(jointLabel) {
  const ui = Object.fromEntries(['run', 'status', 'result', 'points', 'forward', 'return', 'joints']
    .map(name => [name, document.getElementById('ik-' + name)]));
  const xyz = values => values.map(value => Math.round(value)).join(', ');
  const verdict = leg => leg.error || (leg.joints_raw ? '通过' : '前移未通过，回程未计算');
  ui.run.disabled = false;
  ui.run.addEventListener('click', async () => {
    if (ui.run.disabled) return;
    ui.run.disabled = true;
    ui.result.hidden = true;
    ui.status.textContent = '正在读取右臂反馈并计算…';
    try {
      const response = await fetch('/api/kinematics/right-arm/preview', {cache: 'no-store'});
      const result = await response.json();
      if (!response.ok) throw new Error(result.error || '服务返回 HTTP ' + response.status);
      ui.points.textContent = '右肩基座 XYZ（毫米）　起点 (' + xyz(result.start.xyz_mm)
        + ') → 前移目标 (' + xyz(result.target_xyz_mm) + ')';
      ui.forward.textContent = verdict(result.forward);
      ui.return.textContent = verdict(result.return);
      ui.joints.replaceChildren(...result.joint_names.map((name, i) => {
        const row = document.createElement('tr');
        const values = [jointLabel(name), result.start.joints_raw[i],
          result.forward.joints_raw?.[i], result.return.joints_raw?.[i]];
        for (const value of values) row.insertCell().textContent = value ?? '—';
        return row;
      }));
      ui.status.textContent = '本次起点已固定 · bus2 反馈帧 ' + result.stamp_ns;
      ui.result.hidden = false;
    } catch (error) {
      ui.status.textContent = '无法预览：' + error.message;
    } finally {
      ui.run.disabled = false;
    }
  });
}

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

// 夹爪电机行程按现有校准映射至 URDF 张角。控制面板关闭时可用快照的 raw 范围。
export function feedbackAngle(node, bus, feedback) {
  if (feedback?.mode !== 'position' || !Number.isFinite(feedback.position_rad)) return null;
  const mapping = node.displayMapping || node;
  if (!node.range) return (mapping.sign ?? 1)*feedback.position_rad + (mapping.offset ?? 0);
  const jawAngle = fraction => {
    const [closed, open] = node.range;
    return closed + (open-closed)*Math.max(0, Math.min(1, fraction));
  };
  if (mapping.motor_range) {
    const [first, second] = mapping.motor_range;
    if (!Number.isFinite(first) || !Number.isFinite(second) || first === second) return null;
    // 本机夹爪的闭合至张开行程小于半圈，以短弧中点为参考，可跨编码器零点。
    const arc = value => Math.atan2(Math.sin(value), Math.cos(value));
    const travel = arc(second-first);
    if (Math.abs(travel) < 1e-8) return null;
    return jawAngle(0.5 + arc(feedback.position_rad-(first+travel/2))/travel);
  }
  const limit = bus.control?.joints?.find(joint => joint.name === node.joint);
  let span = [limit?.min_position_rad, limit?.max_position_rad];
  let value = feedback.position_rad;
  if (!span.every(Number.isFinite)) {
    span = feedback.target_range_raw;
    value = feedback.position_raw;
  }
  if (!span || !span.every(Number.isFinite) || span[1] <= span[0] || !Number.isFinite(value)) return null;
  return jawAngle((value-span[0])/(span[1]-span[0]));
}

export function createPoseView(canvas, model, onSelect, meshRenderer, reportError) {
  const moving = [];
  for (const node of model.nodes) {
    node.local = origin(node.xyz, node.rpy);
    node.owner = node.parent?.owner;
    if (node.joint) {
      node.owner = node;
      node.pickId = moving.length+1;
      node.angle = node.offset ?? 0;
      node.stamp = null;
      node.fresh = false;
      moving.push(node);
    }
  }
  for (const visual of model.visuals) visual.local = origin(visual.xyz, visual.rpy);
  const defaultAzimuth = -1.1, defaultElevation = 0.21;
  let azimuth = defaultAzimuth, elevation = defaultElevation;
  let zoom = 1, panX = 0, panY = 0, panMode = false;
  let visible = false, frame = 0, selected = null;
  const pointers = new Map();
  let pinch = null;
  const render = meshRenderer(canvas, model, redraw, reportError);

  function redraw() {
    if (visible && !document.hidden && !frame) frame = requestAnimationFrame(draw);
  }

  function paint(hit) {
    const {width, height} = canvas.getBoundingClientRect();
    if (!width || !height) return;
    const ratio = Math.min(devicePixelRatio || 1, 2);
    const pixelsX = Math.round(width*ratio), pixelsY = Math.round(height*ratio);
    if (canvas.width !== pixelsX || canvas.height !== pixelsY) {
      canvas.width = pixelsX;
      canvas.height = pixelsY;
    }
    for (const node of model.nodes) {
      let world = node.parent ? compose(node.parent.world, node.local) : node.local;
      if (node.joint) world = compose(world, rotation(node.axis, node.angle));
      node.world = world;
    }
    for (const visual of model.visuals) visual.world = compose(visual.node.world, visual.local);
    const scale = zoom*Math.min(width/1.16, height/1.48);
    const sx = 2*scale/width, sy = 2*scale/height;
    const ca = Math.cos(azimuth), sa = Math.sin(azimuth);
    const ce = Math.cos(elevation), se = Math.sin(elevation);
    const camera = new Float32Array([
      -sa*sx, -se*ca*sy, -ce*ca/4, 0,
      ca*sx, -se*sa*sy, -ce*sa/4, 0,
      0, ce*sy, -se/4, 0,
      2*panX/width, -0.59*ce*sy-2*panY/height, 0.59*se/4, 1,
    ]);
    return render(camera, moving, selected, ratio, hit);
  }

  function draw() {
    frame = 0;
    paint();
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
    const joint = paint([x, y]);
    if (joint) onSelect(joint);
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
    setOverrides(overrides) {
      for (const node of moving) {
        node.displayMapping = overrides[node.joint] ?? null;
        node.stamp = null;
      }
    },
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
