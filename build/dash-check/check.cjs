// Opens the dashboard (server/static/index.html) in headless Chromium with
// a mocked hub API and checks, at several window sizes, that the page does
// not scroll and that every control of the Settings tab is visible (not cut
// off, not squeezed to nothing). Writes screenshots to OUT.
const fs = require("fs");
const { chromium } = require("playwright-core");
const { state, settings } = require("./fixture.cjs");
const [html, out] = [process.argv[2], process.argv[3]];
const sizes = (process.env.SIZES || "1024x700,1130x900,1366x650,1920x900").split(",").map(s => s.split("x").map(Number));

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM });
  let failed = 0;
  for (const [w, h] of sizes) {
    const page = await browser.newPage({ viewport: { width: w, height: h } });
    await page.addInitScript(() => localStorage.setItem("flowToken", "test"));
    await page.route("**/*", (r) => {
      const u = new URL(r.request().url());
      if (u.pathname === "/") return r.fulfill({ contentType: "text/html", body: fs.readFileSync(html) });
      if (u.pathname === "/api/state") return r.fulfill({ json: state });
      if (u.pathname === "/api/settings") return r.fulfill({ json: settings });
      return r.fulfill({ status: 404, body: "" });
    });
    await page.goto("http://dash.test/");
    await page.waitForSelector("#nodes tr");
    await page.screenshot({ path: `${out}/overview-${w}x${h}.png` });
    await page.click('nav button[data-tab="settings"]');
    await page.waitForSelector("#overrides tr td select");
    await page.waitForTimeout(200);
    await page.screenshot({ path: `${out}/settings-${w}x${h}.png` });
    const r = await page.evaluate(() => {
      const de = document.documentElement;
      const pageScroll = de.scrollHeight > innerHeight + 1 || de.scrollWidth > innerWidth + 1;
      // Clipping by the scroll areas and cards the control sits in.
      const clipped = (e) => {
        const b = e.getBoundingClientRect();
        if (b.width < 1 || b.height < 1) return "no size";
        if (b.left < -1 || b.right > innerWidth + 1) return `outside the window (${Math.round(b.left)}..${Math.round(b.right)})`;
        for (let a = e.parentElement; a; a = a.parentElement) {
          const s = getComputedStyle(a);
          if (s.overflowX === "visible" && s.overflowY === "visible") continue;
          const c = a.getBoundingClientRect();
          if (b.left < c.left - 1 || b.right > c.right + 1) return `cut off by ${a.tagName.toLowerCase()}.${a.className} (${Math.round(b.left)}..${Math.round(b.right)} in ${Math.round(c.left)}..${Math.round(c.right)})`;
          if (a.scrollWidth > a.clientWidth + 1 && s.overflowX !== "hidden") return `needs side scrolling in ${a.tagName.toLowerCase()}.${a.className}`;
        }
        return null;
      };
      const label = (e) => e.id || e.getAttribute("placeholder") || e.textContent.trim().slice(0, 30) || e.tagName;
      const sel = "input:not([type=hidden]), select, textarea, button";
      const tooNarrow = (e, min) => e.getBoundingClientRect().width < min ? `only ${Math.round(e.getBoundingClientRect().width)} px wide` : null;
      const problems = [];
      for (const e of document.querySelectorAll(`#tab-settings ${sel}`)) {
        if (e.offsetParent === null) continue; // hidden on purpose (inherit)
        const p = clipped(e) || (e.matches("textarea, input:not([type=checkbox]):not([type=number]), select") ? tooNarrow(e, 90) : null);
        if (p) problems.push(`${label(e)}: ${p}`);
      }
      // The global network/crossing/cursor controls must be on screen
      // without scrolling the panel.
      const panel = document.querySelector("#s-nets").closest(".card");
      const pb = panel.getBoundingClientRect();
      for (const e of [document.querySelector("#s-nets"), document.querySelector("#s-cross input"), document.querySelector("#s-park")]) {
        const b = e.getBoundingClientRect();
        if (b.top < pb.top - 1 || b.bottom > pb.bottom + 1 || b.bottom > innerHeight) problems.push(`${label(e)}: not visible in its panel without scrolling`);
      }
      return { pageScroll, problems };
    });
    const ok = !r.pageScroll && r.problems.length === 0;
    if (!ok) failed++;
    console.log(`${ok ? "ok  " : "FAIL"} ${w}x${h}${r.pageScroll ? " page scrolls" : ""}`);
    for (const p of r.problems) console.log(`       ${p}`);
    await page.close();
  }
  await browser.close();
  process.exit(failed ? 1 : 0);
})();
