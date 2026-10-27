// Graph tab: speed line plus main buffer, device buffer and CPU (percent), drawn on a canvas.
// Fed live by `studio-graph` batches or by a loaded `.ibg` file.
import type { GraphFile, Sample } from "./api";
import { el, fmtTime } from "./ui";
import { t } from "../i18n";

const COLORS = { speed: "#2f6fde", main: "#2fa84f", device: "#e0a321", cpu: "#9099aa" };

export class GraphView {
  readonly root: HTMLElement;
  private canvas = el("canvas", { class: "graph-canvas" });
  private info = el("div", { class: "graph-info hint" }, t("studio.graph.nochKeineMessdaten"));
  private tip = el("div", { class: "graph-tip", hidden: true });
  private samples: Sample[] = [];
  private sampleMs = 100;
  private hover: number | null = null;
  private pending = false;

  constructor() {
    const legend = el(
      "div",
      { class: "graph-legend" },
      ...(
        [
          ["speed", t("studio.graph.geschwindigkeitX")],
          ["main", t("studio.graph.hauptpuffer")],
          ["device", t("studio.graph.laufwerkspuffer")],
          ["cpu", t("studio.graph.cpu")],
        ] as const
      ).map(([k, t]) => el("span", {}, el("i", { style: `background:${COLORS[k]}` }), t)),
    );
    this.root = el("div", { class: "graph-view" }, legend, el("div", { class: "graph-box" }, this.canvas, this.tip), this.info);
    new ResizeObserver(() => this.redraw()).observe(this.root);
    this.canvas.addEventListener("mousemove", (e) => this.onMove(e));
    this.canvas.addEventListener("mouseleave", () => {
      this.hover = null;
      this.tip.hidden = true;
      this.redraw();
    });
  }

  clear() {
    this.samples = [];
    this.info.textContent = t("studio.graph.nochKeineMessdaten");
    this.redraw();
  }

  /** Appends live samples. */
  push(batch: Sample[]) {
    this.samples.push(...batch);
    this.info.textContent = t("studio.graph.messpunkteAlleMs", { length: this.samples.length, sampleMs: this.sampleMs });
    this.redraw();
  }

  /** Shows a loaded `.ibg`. */
  show(file: GraphFile, title: string) {
    this.samples = file.samples;
    this.sampleMs = file.sampleMs;
    const cfg = Object.fromEntries(file.config);
    const bits = [title, cfg.MEDIA_TYPE, cfg.MEDIA_ID, cfg.DEVICE_MAKEMODEL].filter(Boolean);
    this.info.textContent = t("studio.graph.messpunkte", { bits: bits.join(" · "), length: this.samples.length });
    this.redraw();
  }

  setSampleMs(ms: number) {
    this.sampleMs = ms;
  }

  private redraw() {
    if (this.pending) return;
    this.pending = true;
    requestAnimationFrame(() => {
      this.pending = false;
      this.draw();
    });
  }

  private plot() {
    const w = this.canvas.clientWidth;
    const h = this.canvas.clientHeight;
    return { w, h, l: 44, r: 40, t: 10, b: 24 };
  }

  private onMove(e: MouseEvent) {
    if (this.samples.length === 0) return;
    const p = this.plot();
    const x = e.offsetX - p.l;
    const i = Math.round((x / (p.w - p.l - p.r)) * (this.samples.length - 1));
    if (i < 0 || i >= this.samples.length) return;
    this.hover = i;
    const s = this.samples[i];
    this.tip.hidden = false;
    this.tip.style.left = `${Math.min(e.offsetX + 12, p.w - 170)}px`;
    this.tip.style.top = `${Math.max(e.offsetY - 10, 0)}px`;
    this.tip.textContent = t("studio.graph.xSektorHauptGeraet", { fmtTime: fmtTime((i * this.sampleMs) / 1000), s: s.speed.toFixed(2), lba: s.lba, s2: s.main.toFixed(0), s3: s.device.toFixed(0), s4: s.cpu >= 0 ? t("studio.graph.cpu2", { s: s.cpu.toFixed(0) }) : "" });
    this.redraw();
  }

  private draw() {
    const c = this.canvas;
    const dpr = window.devicePixelRatio || 1;
    const { w, h, l, r, t, b } = this.plot();
    if (w === 0 || h === 0) return;
    c.width = w * dpr;
    c.height = h * dpr;
    const g = c.getContext("2d");
    if (!g) return;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    const css = getComputedStyle(document.documentElement);
    const text = css.getPropertyValue("--muted").trim() || "#687386";
    const grid = css.getPropertyValue("--border").trim() || "#dde2ea";
    g.clearRect(0, 0, w, h);
    g.font = "11px 'Segoe UI', sans-serif";
    const pw = w - l - r;
    const ph = h - t - b;
    const n = this.samples.length;
    const maxSpeed = Math.max(1, Math.ceil(Math.max(0, ...this.samples.map((s) => s.speed)) / 2) * 2);
    const xOf = (i: number) => l + (n <= 1 ? 0 : (i / (n - 1)) * pw);
    // Grid and axes: speed on the left, percent on the right.
    g.strokeStyle = grid;
    g.fillStyle = text;
    for (let k = 0; k <= 4; k++) {
      const y = t + (ph * k) / 4;
      g.beginPath();
      g.moveTo(l, y);
      g.lineTo(l + pw, y);
      g.stroke();
      g.textAlign = "right";
      g.fillText(`${((maxSpeed * (4 - k)) / 4).toFixed(maxSpeed < 8 ? 1 : 0)}x`, l - 6, y + 4);
      g.textAlign = "left";
      g.fillText(`${25 * (4 - k)}%`, l + pw + 6, y + 4);
    }
    if (n > 1) {
      const total = (n * this.sampleMs) / 1000;
      g.textAlign = "center";
      for (let k = 0; k <= 4; k++) g.fillText(fmtTime((total * k) / 4), l + (pw * k) / 4, h - 6);
    }
    const line = (color: string, value: (s: Sample) => number | null, max: number) => {
      g.strokeStyle = color;
      g.lineWidth = 1.5;
      g.beginPath();
      let started = false;
      this.samples.forEach((s, i) => {
        const v = value(s);
        if (v === null) return;
        const y = t + ph - (Math.min(v, max) / max) * ph;
        if (started) g.lineTo(xOf(i), y);
        else g.moveTo(xOf(i), y);
        started = true;
      });
      g.stroke();
    };
    line(COLORS.cpu, (s) => (s.cpu >= 0 ? s.cpu : null), 100);
    line(COLORS.device, (s) => (s.device > 0 ? s.device : null), 100);
    line(COLORS.main, (s) => s.main, 100);
    line(COLORS.speed, (s) => s.speed, maxSpeed);
    if (this.hover !== null && this.hover < n) {
      g.strokeStyle = text;
      g.lineWidth = 1;
      g.beginPath();
      g.moveTo(xOf(this.hover), t);
      g.lineTo(xOf(this.hover), t + ph);
      g.stroke();
    }
  }
}
