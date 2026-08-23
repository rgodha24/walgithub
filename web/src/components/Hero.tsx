import { useEffect, useRef } from "react";
import { Link } from "react-router-dom";

const ARCH_POST = "https://cursor.com/blog/git-at-any-scale";
const TURBOPUFFER = "https://turbopuffer.com/blog/turbopuffer";

/** Landing banner: what this is, with an animated commit-DAG backdrop. */
export function Hero() {
  return (
    <section className="hero" aria-labelledby="hero-title">
      <DagCanvas />
      <div className="hero-body">
        <h1 id="hero-title">walgit</h1>
        <p className="hero-lede">A git server that is a single binary in front of an object store.</p>
        <p className="hero-text">
          walgit is an implementation of the ideas in the{" "}
          <a href={ARCH_POST} target="_blank" rel="noreferrer" className="hero-link">
            git hosting architecture of Origin (Cursor)
          </a>{" "}
          and our friends at{" "}
          <a href={TURBOPUFFER} target="_blank" rel="noreferrer" className="hero-link">
            Turbopuffer
          </a>{" "}
          who have pioneered GCS/S3 based share-nothing infrastructure to scale.
        </p>
        <div className="hero-actions">
          <Link to="/api" className="btn btn-primary">
            Explore the API
          </Link>
          <a href={ARCH_POST} target="_blank" rel="noreferrer" className="btn">
            Read “Git at any scale” ↗
          </a>
        </div>
      </div>
    </section>
  );
}

/**
 * Animated backdrop: commits drifting along lanes, merging and branching —
 * a live commit graph, with a write-ahead-log "append" pulse sweeping through.
 * Pure canvas, ~60 nodes, pauses when the tab is hidden, static when the user
 * prefers reduced motion.
 */
function DagCanvas() {
  const ref = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    const canvas = ref.current!;
    const ctx = canvas.getContext("2d")!;
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const styles = getComputedStyle(canvas);
    const accent = styles.getPropertyValue("--accent").trim() || "#0969da";
    const add = styles.getPropertyValue("--add").trim() || "#1a7f37";
    const del = styles.getPropertyValue("--del").trim() || "#cf222e";
    const palette = [accent, add, "#8250df", del, "#bf8700"];

    type Node = { x: number; y: number; lane: number; r: number; c: string; parents: Node[]; born: number };
    let w = 0;
    let h = 0;
    let dpr = 1;
    const LANES = 7;
    const nodes: Node[] = [];
    const speed = 22; // px/s drift to the left
    let last = performance.now();
    let pulseX = -1;
    let raf = 0;

    const laneY = (lane: number) => ((lane + 1) / (LANES + 1)) * h;
    const spawn = (x: number, now: number) => {
      const lane = Math.floor(Math.random() * LANES);
      const n: Node = { x, y: laneY(lane), lane, r: 3 + Math.random() * 2.5, c: palette[lane % palette.length]!, parents: [], born: now };
      // Parent: most recent node in the same lane (history), sometimes a merge from a neighbour lane.
      const sameLane = nodes.findLast((m) => m.lane === lane);
      if (sameLane) n.parents.push(sameLane);
      if (Math.random() < 0.35) {
        const other = nodes.findLast((m) => Math.abs(m.lane - lane) === 1 && m.x < x - 20);
        if (other) n.parents.push(other);
      }
      nodes.push(n);
    };

    const resize = () => {
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      w = canvas.clientWidth;
      h = canvas.clientHeight;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      for (const n of nodes) n.y = laneY(n.lane);
      if (nodes.length === 0) {
        const now = performance.now();
        for (let x = 20; x < w + 60; x += 28 + Math.random() * 40) spawn(x, now - 5000);
      }
    };

    const draw = (now: number) => {
      const dt = reduce ? 0 : Math.min(0.05, (now - last) / 1000);
      last = now;
      for (const n of nodes) n.x -= speed * dt;
      while (nodes.length && nodes[0]!.x < -40) nodes.shift();
      const rightmost = nodes.reduce((m, n) => Math.max(m, n.x), 0);
      if (rightmost < w + 20) spawn(rightmost + 26 + Math.random() * 44, now);
      // WAL append pulse sweeping left→right every few seconds.
      pulseX = reduce ? -1 : ((now / 4000) % 1.6) * (w + 200) - 100;

      ctx.clearRect(0, 0, w, h);
      // Lane guides.
      ctx.lineWidth = 1;
      for (let l = 0; l < LANES; l++) {
        ctx.strokeStyle = "rgba(120,130,145,0.10)";
        ctx.beginPath();
        ctx.moveTo(0, laneY(l));
        ctx.lineTo(w, laneY(l));
        ctx.stroke();
      }
      // Edges (curved, like a commit graph).
      ctx.lineWidth = 1.5;
      for (const n of nodes) {
        for (const p of n.parents) {
          const age = Math.min(1, (now - n.born) / 900);
          ctx.strokeStyle = hexA(n.c, 0.35 * age);
          ctx.beginPath();
          ctx.moveTo(p.x, p.y);
          const mx = (p.x + n.x) / 2;
          ctx.bezierCurveTo(mx, p.y, mx, n.y, n.x, n.y);
          ctx.stroke();
        }
      }
      // Nodes.
      for (const n of nodes) {
        const age = Math.min(1, (now - n.born) / 600);
        const near = pulseX >= 0 ? Math.max(0, 1 - Math.abs(n.x - pulseX) / 90) : 0;
        const r = n.r * (0.6 + 0.4 * age) + near * 2.5;
        ctx.fillStyle = hexA(n.c, 0.55 + 0.45 * near);
        ctx.beginPath();
        ctx.arc(n.x, n.y, r, 0, Math.PI * 2);
        ctx.fill();
        ctx.strokeStyle = "rgba(255,255,255,0.9)";
        ctx.lineWidth = 1.25;
        ctx.stroke();
      }
      // The pulse itself.
      if (pulseX >= 0) {
        const g = ctx.createLinearGradient(pulseX - 120, 0, pulseX + 20, 0);
        g.addColorStop(0, "rgba(9,105,218,0)");
        g.addColorStop(0.8, "rgba(9,105,218,0.10)");
        g.addColorStop(1, "rgba(9,105,218,0)");
        ctx.fillStyle = g;
        ctx.fillRect(pulseX - 120, 0, 140, h);
      }
      if (!reduce) raf = requestAnimationFrame(draw);
    };

    const onVis = () => {
      cancelAnimationFrame(raf);
      if (!document.hidden) {
        last = performance.now();
        raf = requestAnimationFrame(draw);
      }
    };
    const ro = new ResizeObserver(resize);
    ro.observe(canvas);
    resize();
    raf = requestAnimationFrame(draw);
    document.addEventListener("visibilitychange", onVis);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      document.removeEventListener("visibilitychange", onVis);
    };
  }, []);
  return <canvas ref={ref} className="hero-canvas" aria-hidden />;
}

function hexA(hex: string, a: number): string {
  const m = /^#?([\da-f]{2})([\da-f]{2})([\da-f]{2})$/i.exec(hex);
  if (!m) return hex;
  return `rgba(${parseInt(m[1]!, 16)},${parseInt(m[2]!, 16)},${parseInt(m[3]!, 16)},${a.toFixed(3)})`;
}
