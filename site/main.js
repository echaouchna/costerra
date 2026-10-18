// costerra — homepage interactions. Vanilla JS, no dependencies.
(() => {
  "use strict";

  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const $ = (s, el = document) => el.querySelector(s);
  const $$ = (s, el = document) => [...el.querySelectorAll(s)];
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const fmt = (n, dec = 0) =>
    n.toLocaleString("en-US", { minimumFractionDigits: dec, maximumFractionDigits: dec });

  /* ───────────── Navigation ───────────── */
  const nav = $("#nav");
  const onScroll = () => nav.classList.toggle("scrolled", window.scrollY > 40);
  onScroll();
  window.addEventListener("scroll", onScroll, { passive: true });

  const burger = $("#burger");
  const links = $("#nav-links");
  burger.addEventListener("click", () => {
    const open = links.classList.toggle("open");
    burger.setAttribute("aria-expanded", String(open));
  });
  $$("a", links).forEach((a) =>
    a.addEventListener("click", () => {
      links.classList.remove("open");
      burger.setAttribute("aria-expanded", "false");
    })
  );

  const navMap = new Map($$("a", links).map((a) => [a.getAttribute("href").slice(1), a]));
  const sectionObs = new IntersectionObserver(
    (entries) =>
      entries.forEach((e) => {
        if (!e.isIntersecting) return;
        navMap.forEach((a) => a.classList.remove("active"));
        navMap.get(e.target.id)?.classList.add("active");
      }),
    { rootMargin: "-45% 0px -50% 0px" }
  );
  navMap.forEach((_, id) => {
    const s = document.getElementById(id);
    if (s) sectionObs.observe(s);
  });

  /* ───────────── Odometers ───────────── */
  $$(".odo").forEach((el) => {
    const value = el.dataset.odo;
    el.setAttribute("aria-label", value);
    el.innerHTML = [...value]
      .map((ch) => {
        if (!/\d/.test(ch)) return `<span>${ch}</span>`;
        const cells = Array.from({ length: 20 }, (_, i) => `<span>${i % 10}</span>`).join("");
        return `<span class="odo-d" aria-hidden="true"><span class="odo-s" data-d="${ch}">${cells}</span></span>`;
      })
      .join("");
  });
  const rollOdo = (el) => {
    const strips = $$(".odo-s", el);
    strips.forEach((s, i) => {
      s.style.transitionDelay = `${(strips.length - 1 - i) * 0.12}s`;
      s.style.transform = `translateY(${-(10 + Number(s.dataset.d)) * 1.05}em)`;
    });
    // Once settled, swap the strips for plain text so proportional digits close up.
    setTimeout(() => { el.textContent = el.dataset.odo; }, reduced ? 0 : 2200 + strips.length * 120 + 100);
  };

  /* ───────────── Count-up numbers ───────────── */
  function countUp(el) {
    if (el.dataset.done) return;
    el.dataset.done = "1";
    const target = parseFloat(el.dataset.count);
    const dec = parseInt(el.dataset.decimals || "0", 10);
    if (reduced) { el.textContent = fmt(target, dec); return; }
    const t0 = performance.now();
    const dur = 1800;
    const step = (t) => {
      const p = Math.min(1, (t - t0) / dur);
      const e = p === 1 ? 1 : 1 - Math.pow(2, -10 * p);
      el.textContent = fmt(target * e, dec);
      if (p < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }

  /* ───────────── Reveal on scroll ───────────── */
  const revealObs = new IntersectionObserver(
    (entries) =>
      entries.forEach((e) => {
        if (!e.isIntersecting) return;
        e.target.classList.add("in");
        $$("[data-count]", e.target).forEach(countUp);
        $$(".odo", e.target).forEach(rollOdo);
        if (e.target.classList.contains("station") && !document.querySelector(".chain")?.offsetParent) e.target.classList.add("hit");
        revealObs.unobserve(e.target);
      }),
    { threshold: 0.15, rootMargin: "0px 0px -40px 0px" }
  );
  $$(".reveal").forEach((el) => revealObs.observe(el));

  /* ───────────── Hero word rotator + scribble ───────────── */
  const rot = $("#rotator");
  const scribble = $(".scribble");
  const redraw = () => {
    scribble.classList.remove("draw");
    void scribble.getBoundingClientRect();
    scribble.classList.add("draw");
  };
  setTimeout(redraw, 700);
  if (rot && !reduced) {
    const words = ["apply", "merge", "deploy", "ship"];
    let i = 0;
    setInterval(async () => {
      rot.classList.remove("in");
      rot.classList.add("out");
      await sleep(360);
      i = (i + 1) % words.length;
      rot.textContent = words[i];
      rot.classList.remove("out");
      rot.classList.add("in");
      redraw();
    }, 3200);
  }

  /* ───────────── Receipt printer ───────────── */
  const receipt = $("#receipt");
  const stamp = $("#stamp");
  const printer = $(".printer");
  const now = new Date();
  const pad = (n) => String(n).padStart(2, "0");
  const stampDate = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())} ${pad(now.getHours())}:${pad(now.getMinutes())}`;
  const dashes = "- ".repeat(40);
  const lines = [
    { c: "title c", l: "COSTERRA", d: 260 },
    { c: "c dim", l: "CLOUD COST ESTIMATE", d: 120 },
    { c: "c dim", l: "live/ · 15 projects · 350 resources", d: 120 },
    { c: "c dim", l: `${stampDate} · no. 0042`, d: 200 },
    { c: "rule", l: dashes, d: 120 },
    { c: "b", l: "google_compute_instance.app ×4", r: "$1,012.47", d: 150 },
    { c: "dim", l: "  n2-standard-8 · 2,920 h", r: "$999.27", d: 90 },
    { c: "dim", l: "  incl. 20% sustained use disc.", d: 90 },
    { c: "dim", l: "  pd-balanced · 120 GiB-mo", r: "$13.20", d: 140 },
    { c: "b", l: "google_sql_database_instance.main", r: "$642.18", d: 150 },
    { c: "dim", l: "  Enterprise · HA · 4 vCPU", r: "$530.81", d: 90 },
    { c: "dim", l: "  memory · 11,680 GiB-h", r: "$111.37", d: 140 },
    { c: "b", l: "google_storage_bucket.assets", r: "$9.00", d: 150 },
    { c: "dim", l: "  usage-based · medium profile", d: 140 },
    { c: "", l: "… 14 more projects", r: "$2,554.71", d: 240 },
    { c: "rule", l: dashes, d: 120 },
    { c: "", l: "SUBTOTAL", r: "$4,218.36", d: 200 },
    { c: "red", l: "SAVINGS FOUND (6)", r: "−$336.06", d: 200 },
    { c: "rule", l: "=".repeat(80), d: 120 },
    { c: "big", l: "IF APPLIED / MONTH", r: "$3,882.30", d: 260 },
    { c: "rule", l: dashes, d: 120 },
    { c: "c dim", l: "61 ms · no init · no credentials", d: 120 },
    { c: "c dim", l: "THANK YOU FOR NOT OVERSPENDING", d: 160 },
    { c: "barcode", l: "", d: 0 },
  ];
  const lineEl = (ln) => {
    const div = document.createElement("div");
    div.className = `rc ${ln.c}`;
    if (ln.c === "barcode") {
      let seed = 42;
      const rnd = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);
      for (let i = 0; i < 46; i++) {
        const b = document.createElement("i");
        b.style.width = `${1 + Math.floor(rnd() * 3)}px`;
        b.style.marginRight = `${1 + Math.floor(rnd() * 3)}px`;
        div.appendChild(b);
      }
      return div;
    }
    const l = document.createElement("span");
    l.textContent = ln.l;
    div.appendChild(l);
    if (ln.r) {
      const r = document.createElement("span");
      r.textContent = ln.r;
      div.appendChild(r);
    }
    return div;
  };

  let printRun = 0;
  async function printReceipt() {
    const id = ++printRun;
    receipt.innerHTML = "";
    stamp.classList.remove("hit");
    if (reduced) {
      lines.forEach((ln) => receipt.appendChild(lineEl(ln)));
      stamp.style.opacity = ".92";
      stamp.style.transform = "rotate(-13deg)";
      return;
    }
    printer.classList.add("busy");
    await sleep(500);
    for (const ln of lines) {
      if (id !== printRun) return;
      receipt.appendChild(lineEl(ln));
      await sleep(ln.d);
    }
    printer.classList.remove("busy");
    await sleep(450);
    if (id !== printRun) return;
    stamp.classList.add("hit");
    const wrap = receipt.parentElement;
    setTimeout(() => {
      wrap.classList.remove("shake");
      void wrap.offsetWidth;
      wrap.classList.add("shake");
    }, 280);
  }
  if (receipt) {
    new IntersectionObserver((entries, obs) => {
      if (entries[0].isIntersecting) { printReceipt(); obs.disconnect(); }
    }, { threshold: 0.2 }).observe(receipt.parentElement);
    $("#reprint").addEventListener("click", printReceipt);
  }

  /* ───────────── Topographic contours (live, cursor-reactive) ───────────── */
  function hash(x, y, z) {
    let h = (Math.imul(x, 374761393) + Math.imul(y, 668265263) + Math.imul(z, 1442695041)) | 0;
    h = Math.imul(h ^ (h >>> 13), 1274126177);
    h ^= h >>> 16;
    return (h >>> 0) / 4294967295;
  }
  const smooth = (t) => t * t * (3 - 2 * t);
  function noise3(x, y, z) {
    const xi = Math.floor(x), yi = Math.floor(y), zi = Math.floor(z);
    const xf = smooth(x - xi), yf = smooth(y - yi), zf = smooth(z - zi);
    const lerp = (a, b, t) => a + (b - a) * t;
    const c = (dx, dy, dz) => hash(xi + dx, yi + dy, zi + dz);
    return lerp(
      lerp(lerp(c(0, 0, 0), c(1, 0, 0), xf), lerp(c(0, 1, 0), c(1, 1, 0), xf), yf),
      lerp(lerp(c(0, 0, 1), c(1, 0, 1), xf), lerp(c(0, 1, 1), c(1, 1, 1), xf), yf),
      zf
    );
  }
  const fbm = (x, y, z) => noise3(x, y, z) * 0.6 + noise3(x * 2.1, y * 2.1, z * 1.3 + 7) * 0.28 + noise3(x * 4.3, y * 4.3, z * 1.7 + 13) * 0.12;

  // Segments per marching-squares case: edges 0=top 1=right 2=bottom 3=left.
  const CASES = [[], [[3, 2]], [[2, 1]], [[3, 1]], [[0, 1]], [[0, 1], [3, 2]], [[0, 2]], [[0, 3]], [[0, 3]], [[0, 2]], [[0, 3], [2, 1]], [[0, 1]], [[3, 1]], [[2, 1]], [[3, 2]], []];

  function contours(canvas) {
    const theme = canvas.dataset.contours;
    const col = theme === "paper" ? "251,249,244" : "22,20,15";
    const base = theme === "paper" ? 0.2 : 0.13;
    const strong = theme === "paper" ? 0.42 : 0.3;
    const host = canvas.parentElement;
    const ctx = canvas.getContext("2d");
    const CELL = 14;
    const levels = [];
    for (let v = 0.12; v < 1.25; v += 0.045) levels.push(v);
    let w = 0, h = 0, cols = 0, rows = 0, grid = new Float32Array(0);
    let visible = false, raf = 0, last = 0;
    const mouse = { x: -9999, y: -9999, tx: -9999, ty: -9999, amp: 0, tamp: 0 };
    const seed = theme === "paper" ? 50 : 0;

    const resize = () => {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      w = host.clientWidth; h = host.clientHeight;
      canvas.width = w * dpr; canvas.height = h * dpr;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      cols = Math.ceil(w / CELL) + 1; rows = Math.ceil(h / CELL) + 1;
      grid = new Float32Array(cols * rows);
    };

    const frame = (t) => {
      const z = seed + t * 0.000045;
      mouse.x += (mouse.tx - mouse.x) * 0.08;
      mouse.y += (mouse.ty - mouse.y) * 0.08;
      mouse.amp += (mouse.tamp - mouse.amp) * 0.05;
      const s2 = 2 * 130 * 130;
      for (let j = 0; j < rows; j++) {
        for (let i = 0; i < cols; i++) {
          const px = i * CELL, py = j * CELL;
          let v = fbm(px / 300, py / 300, z);
          if (mouse.amp > 0.01) {
            const dx = px - mouse.x, dy = py - mouse.y;
            v += mouse.amp * Math.exp(-(dx * dx + dy * dy) / s2);
          }
          grid[j * cols + i] = v;
        }
      }
      ctx.clearRect(0, 0, w, h);
      const pt = (e, i, j, a, b, c, d, L) => {
        const x = i * CELL, y = j * CELL;
        switch (e) {
          case 0: return [x + CELL * ((L - a) / (b - a)), y];
          case 1: return [x + CELL, y + CELL * ((L - b) / (c - b))];
          case 2: return [x + CELL * ((L - d) / (c - d)), y + CELL];
          default: return [x, y + CELL * ((L - a) / (d - a))];
        }
      };
      levels.forEach((L, li) => {
        const isIndex = li % 5 === 0;
        const isPeak = li === levels.length - 4 && theme !== "paper";
        ctx.beginPath();
        for (let j = 0; j < rows - 1; j++) {
          for (let i = 0; i < cols - 1; i++) {
            const a = grid[j * cols + i], b = grid[j * cols + i + 1];
            const c = grid[(j + 1) * cols + i + 1], d = grid[(j + 1) * cols + i];
            const k = (a > L ? 8 : 0) | (b > L ? 4 : 0) | (c > L ? 2 : 0) | (d > L ? 1 : 0);
            if (k === 0 || k === 15) continue;
            for (const [e1, e2] of CASES[k]) {
              const p1 = pt(e1, i, j, a, b, c, d, L), p2 = pt(e2, i, j, a, b, c, d, L);
              ctx.moveTo(p1[0], p1[1]);
              ctx.lineTo(p2[0], p2[1]);
            }
          }
        }
        if (isPeak) { ctx.strokeStyle = "rgba(229,72,47,.55)"; ctx.lineWidth = 1.4; }
        else { ctx.strokeStyle = `rgba(${col},${isIndex ? strong : base})`; ctx.lineWidth = isIndex ? 1.2 : 0.8; }
        ctx.stroke();
      });
    };

    const loop = (t) => {
      raf = requestAnimationFrame(loop);
      if (t - last < 42) return; // ~24 fps is plenty for slow terrain
      last = t;
      frame(t);
    };
    const start = () => { if (!raf && visible && !document.hidden) raf = requestAnimationFrame(loop); };
    const stop = () => { cancelAnimationFrame(raf); raf = 0; };

    resize();
    new ResizeObserver(() => { resize(); frame(performance.now()); }).observe(host);
    if (reduced) { frame(0); return; }
    new IntersectionObserver((e) => { visible = e[0].isIntersecting; visible ? start() : stop(); }).observe(host);
    document.addEventListener("visibilitychange", () => (document.hidden ? stop() : start()));
    host.addEventListener("pointermove", (e) => {
      const r = host.getBoundingClientRect();
      mouse.tx = e.clientX - r.left; mouse.ty = e.clientY - r.top;
      if (mouse.amp < 0.01) { mouse.x = mouse.tx; mouse.y = mouse.ty; }
      mouse.tamp = 0.38;
    });
    host.addEventListener("pointerleave", () => { mouse.tamp = 0; });
  }
  $$("canvas[data-contours]").forEach(contours);

  /* ───────────── Survey chain (scroll-linked) ───────────── */
  const survey = $("#survey");
  const fill = $("#chain-fill");
  if (survey && fill) {
    const stations = $$(".station", survey);
    let ticking = false;
    const update = () => {
      ticking = false;
      if (!fill.parentElement.offsetParent) return; // chain hidden on small screens
      const r = survey.getBoundingClientRect();
      const vh = window.innerHeight;
      const p = Math.max(0, Math.min(1, (vh * 0.85 - r.top) / (vh * 0.6)));
      fill.style.width = `${p * 100}%`;
      stations.forEach((s) => {
        const at = (s.offsetLeft + 13) / survey.clientWidth;
        s.classList.toggle("hit", p >= at);
      });
    };
    const req = () => { if (!ticking) { ticking = true; requestAnimationFrame(update); } };
    window.addEventListener("scroll", req, { passive: true });
    window.addEventListener("resize", req);
    update();
    // Small screens: light up each station as it scrolls into view.
    stations.forEach((s) => revealObs.observe(s));
  }

  /* ───────────── Tabs ───────────── */
  $$("[data-tabs]").forEach((box) => {
    const buttons = $$(":scope > .tab-list button", box);
    const panels = $$(":scope > .tab-panel", box);
    buttons.forEach((b) =>
      b.addEventListener("click", () => {
        buttons.forEach((x) => { x.classList.toggle("on", x === b); x.setAttribute("aria-selected", String(x === b)); });
        panels.forEach((p) => p.classList.toggle("on", p.dataset.panel === b.dataset.tab));
      })
    );
  });

  /* ───────────── Copy buttons ───────────── */
  $$("[data-copy-target]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const src = document.getElementById(btn.dataset.copyTarget);
      const keepComments = /code-(gh|gl|policy)/.test(src.id);
      const text = src.textContent
        .split("\n")
        .filter((l) => keepComments || !/^\s*#/.test(l))
        .map((l) => l.replace(/^\$ /, ""))
        .join("\n")
        .trim();
      try {
        await navigator.clipboard.writeText(text);
      } catch {
        const ta = Object.assign(document.createElement("textarea"), { value: text });
        document.body.appendChild(ta); ta.select(); document.execCommand("copy"); ta.remove();
      }
      btn.classList.add("done");
      btn.textContent = "copied";
      setTimeout(() => { btn.classList.remove("done"); btn.textContent = "copy"; }, 1600);
    });
  });

  /* ───────────── Usage profile toggle ───────────── */
  $$("[data-profile]").forEach((box) => {
    const values = { small: { bucket: 0.43, total: 0.72 }, medium: { bucket: 9, total: 15.21 }, large: { bucket: 90, total: 151 } };
    const order = ["small", "medium", "large"];
    const buttons = $$(".seg button", box);
    const glider = $(".seg-glider", box);
    const outs = { bucket: $('[data-val="bucket"]', box), total: $('[data-val="total"]', box) };
    let current = "medium", auto = true, visible = false;

    const tween = (el, from, to) => {
      if (reduced) { el.textContent = "$" + fmt(to, 2); return; }
      const t0 = performance.now();
      const step = (t) => {
        const p = Math.min(1, (t - t0) / 650);
        el.textContent = "$" + fmt(from + (to - from) * (1 - Math.pow(1 - p, 3)), 2);
        if (p < 1) requestAnimationFrame(step);
      };
      requestAnimationFrame(step);
    };
    const select = (p) => {
      const prev = values[current];
      current = p;
      buttons.forEach((b) => b.classList.toggle("on", b.dataset.p === p));
      glider.style.transform = `translateX(${order.indexOf(p) * 100}%)`;
      tween(outs.bucket, prev.bucket, values[p].bucket);
      tween(outs.total, prev.total, values[p].total);
    };
    select("medium");
    buttons.forEach((b) => b.addEventListener("click", () => { auto = false; select(b.dataset.p); }));
    new IntersectionObserver((e) => { visible = e[0].isIntersecting; }).observe(box);
    if (!reduced) setInterval(() => { if (auto && visible) select(order[(order.indexOf(current) + 1) % 3]); }, 2600);
  });
})();
