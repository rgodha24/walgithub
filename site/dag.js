// Animated backdrop: commits drifting along lanes with a WAL "append" pulse. Pure canvas,
// pauses when hidden, static under prefers-reduced-motion.
(() => {
  const canvas = document.getElementById("dag");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const css = getComputedStyle(document.documentElement);
  const palette = [css.getPropertyValue("--accent").trim(), css.getPropertyValue("--add").trim(), "#8250df", "#bf8700"];
  let w = 0, h = 0, dpr = 1, lanes = [], nodes = [], t = 0;
  const resize = () => {
    dpr = Math.min(devicePixelRatio || 1, 2);
    w = canvas.clientWidth; h = canvas.clientHeight;
    canvas.width = w * dpr; canvas.height = h * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    const n = Math.max(4, Math.floor(h / 70));
    lanes = Array.from({ length: n }, (_, i) => ({ y: (i + 0.5) * (h / n), color: palette[i % palette.length] }));
    nodes = Array.from({ length: 60 }, () => ({ lane: Math.floor(Math.random() * n), x: Math.random() * w, r: 3 + Math.random() * 2, speed: 12 + Math.random() * 20 }));
  };
  const draw = (dt) => {
    ctx.clearRect(0, 0, w, h);
    ctx.lineWidth = 1;
    for (const l of lanes) { ctx.strokeStyle = l.color + "55"; ctx.beginPath(); ctx.moveTo(0, l.y); ctx.lineTo(w, l.y); ctx.stroke(); }
    for (const n of nodes) {
      n.x += n.speed * dt; if (n.x > w + 10) { n.x = -10; n.lane = Math.floor(Math.random() * lanes.length); }
      const l = lanes[n.lane];
      ctx.fillStyle = l.color; ctx.beginPath(); ctx.arc(n.x, l.y, n.r, 0, Math.PI * 2); ctx.fill();
    }
    // the append pulse sweeping left → right
    const px = ((t * 120) % (w + 200)) - 100;
    const g = ctx.createLinearGradient(px - 80, 0, px + 80, 0);
    g.addColorStop(0, "rgba(255,255,255,0)"); g.addColorStop(0.5, "rgba(255,255,255,0.18)"); g.addColorStop(1, "rgba(255,255,255,0)");
    ctx.fillStyle = g; ctx.fillRect(px - 80, 0, 160, h);
  };
  let last = performance.now();
  const frame = (now) => {
    const dt = Math.min(0.05, (now - last) / 1000); last = now; t += dt;
    if (!document.hidden) draw(dt);
    if (!reduce) requestAnimationFrame(frame);
  };
  new ResizeObserver(resize).observe(canvas);
  resize(); draw(0);
  if (!reduce) requestAnimationFrame(frame);
})();
