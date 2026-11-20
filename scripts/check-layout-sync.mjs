// Unit & integration checks for frontend/src/layout.ts (syncLayoutWithTitles, draftLayout)
// and frontend/src/project.ts (legacy v1 layoutEdited backwards compatibility).
// Pure Node (uses node:module registerHooks + built-in TypeScript type stripping).
import assert from "node:assert/strict";
import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier.startsWith("@tauri-apps/")) {
      return { url: `tauri-stub:${specifier}`, shortCircuit: true };
    }
    if (
      (specifier.startsWith("./") || specifier.startsWith("../")) &&
      !/\.(ts|tsx|js|mjs|cjs|json)$/.test(specifier)
    ) {
      return nextResolve(`${specifier}.ts`, context);
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    if (url.startsWith("tauri-stub:")) {
      return {
        format: "module",
        source:
          "export const invoke = async () => null;" +
          "export const listen = async () => () => {};" +
          "export const open = async () => null;" +
          "export const save = async () => null;",
        shortCircuit: true,
      };
    }
    return nextLoad(url, context);
  },
});

const { draftLayout, syncLayoutWithTitles, isButton, maxButtons, isBdFrame, titlesKey } = await import(
  "../frontend/src/layout.ts"
);
const { parseProject, PROJECT_FORMAT, PROJECT_VERSION } = await import("../frontend/src/project.ts");

function makeTitle(index, path, opts = {}) {
  const chapters = opts.chapters ?? 1;
  return {
    index,
    label: opts.label ?? `Video ${index}`,
    path,
    chapters,
    durationSecs: opts.durationSecs ?? 600,
    chapterNames: Array.from({ length: chapters }, (_, i) => `Chapter ${i + 1}`),
    chapterStarts: Array.from({ length: chapters }, (_, i) => i * 120),
    audioTracks: opts.audioTracks ?? [{ source: 1, label: "English AC-3" }],
    subtitleTracks: opts.subtitleTracks ?? [],
  };
}

function aabb(e) {
  const deg = ((e.rotation % 360) + 360) % 360;
  if (deg === 0) return { x0: e.x, y0: e.y, x1: e.x + e.w, y1: e.y + e.h };
  const rad = (deg * Math.PI) / 180;
  const cos = Math.abs(Math.cos(rad));
  const sin = Math.abs(Math.sin(rad));
  const cx = e.x + e.w / 2;
  const cy = e.y + e.h / 2;
  const hw = (e.w * cos + e.h * sin) / 2;
  const hh = (e.w * sin + e.h * cos) / 2;
  return { x0: cx - hw, y0: cy - hh, x1: cx + hw, y1: cy + hh };
}

/** Mirrors DVD even-y Rect::from_template widening / BD pixel box to catch near-overlaps. */
function targetButtonRect(e, canvas, tv) {
  if (isBdFrame(tv)) {
    const b = aabb(e);
    return {
      x0: Math.max(0, Math.round(b.x0)),
      y0: Math.max(0, Math.round(b.y0)),
      x1: Math.min(canvas.width, Math.round(b.x1)),
      y1: Math.min(canvas.height, Math.round(b.y1)),
    };
  }
  const b = aabb(e);
  return {
    x0: Math.max(0, Math.round(b.x0)),
    x1: Math.min(canvas.width, Math.round(b.x1)),
    y0: Math.max(0, Math.round(b.y0)) & ~1,
    y1: Math.min(canvas.height, Math.round(b.y1) + 1) & ~1,
  };
}

function rectsOverlap(a, b) {
  return a.x0 < b.x1 && b.x0 < a.x1 && a.y0 < b.y1 && b.y0 < a.y1;
}

function assertValidLayout(layout, titles, tv, label) {
  assert.ok(layout.menus.length > 0, `${label}: no menus`);
  const titleMenus = layout.menus.filter((m) => m.role === "title");
  assert.ok(titleMenus.length >= 1, `${label}: needs at least one title menu`);
  const menuIds = new Set(layout.menus.map((m) => m.id));
  assert.equal(menuIds.size, layout.menus.length, `${label}: duplicate menu IDs`);

  for (const m of layout.menus) {
    if (m.role === "root" || m.role === "settings") {
      assert.ok(
        typeof m.title === "number" && m.title >= 1 && m.title <= titles.length,
        `${label}: invalid VTSM menu title ${m.title} in ${m.name}`,
      );
    }
    if (m.background.video && !m.background.video.path) {
      assert.ok(
        m.background.video.title >= 1 && m.background.video.title <= titles.length,
        `${label}: dangling bg video title ${m.background.video.title}`,
      );
    }
    if (m.audio.type === "title") {
      assert.ok(
        m.audio.title >= 1 && m.audio.title <= titles.length,
        `${label}: dangling menu audio title ${m.audio.title}`,
      );
    }

    const buttons = m.elements.filter(isButton);
    assert.ok(
      buttons.length <= maxButtons(tv),
      `${label}: menu ${m.name} exceeds button limit (${buttons.length} > ${maxButtons(tv)})`,
    );

    for (const e of m.elements) {
      const box = aabb(e);
      assert.ok(
        box.x0 >= -0.5 && box.y0 >= -0.5 && box.x1 <= layout.canvas.width + 0.5 && box.y1 <= layout.canvas.height + 0.5,
        `${label}: element ${e.id} out of canvas (${JSON.stringify(box)} vs ${JSON.stringify(layout.canvas)})`,
      );
      if (e.type === "video" && !e.path) {
        assert.ok(e.title >= 1 && e.title <= titles.length, `${label}: dangling video element title ${e.title}`);
      }
      if (e.action) {
        if (e.action.type === "playTitle") {
          assert.ok(e.action.title >= 1 && e.action.title <= titles.length, `${label}: bad playTitle ${e.action.title}`);
        } else if (e.action.type === "playChapter") {
          const t = titles[e.action.title - 1];
          assert.ok(t && e.action.chapter >= 1 && e.action.chapter <= t.chapters, `${label}: bad playChapter`);
        } else if (e.action.type === "menu") {
          assert.ok(menuIds.has(e.action.menu), `${label}: dangling menu link ${e.action.menu}`);
        }
      }
    }

    const rects = buttons.map((b) => ({ id: b.id, rect: targetButtonRect(b, layout.canvas, tv) }));
    for (let i = 0; i < rects.length; i++) {
      for (let j = i + 1; j < rects.length; j++) {
        assert.ok(
          !rectsOverlap(rects[i].rect, rects[j].rect),
          `${label}: overlapping buttons in ${m.name}: ${rects[i].id} ${JSON.stringify(rects[i].rect)} vs ${rects[j].id} ${JSON.stringify(rects[j].rect)}`,
        );
      }
    }
  }
}

// ---------------------------------------------------------------------------
// (a) Pristine auto-draft on 1, 2, 7 titles across PAL, NTSC, BD1080
// ---------------------------------------------------------------------------
for (const tv of ["PAL", "NTSC", "BD1080"]) {
  for (const count of [1, 2, 7]) {
    const titles = Array.from({ length: count }, (_, i) =>
      makeTitle(i + 1, `/movies/v${i + 1}.mp4`, {
        chapters: i === 0 ? 3 : 1,
        audioTracks:
          i === 1
            ? [
                { source: 1, label: "English" },
                { source: 2, label: "German" },
              ]
            : [{ source: 1, label: "English" }],
      }),
    );
    const layout = draftLayout(titles, tv, "My Disc");
    assertValidLayout(layout, titles, tv, `draft ${tv} n=${count}`);
    const titlePages = layout.menus.filter((m) => m.role === "title");
    assert.equal(titlePages.length, count > 6 ? 2 : 1, `${tv} n=${count} title page count`);
    if (tv === "BD1080") {
      assert.deepEqual(layout.canvas, { width: 1920, height: 1080 });
      const firstVid = titlePages[0].elements.find((e) => e.type === "video");
      assert.equal(firstVid.w, 400);
      assert.equal(firstVid.h, 225);
    } else if (tv === "NTSC") {
      assert.deepEqual(layout.canvas, { width: 720, height: 480 });
    } else {
      assert.deepEqual(layout.canvas, { width: 720, height: 576 });
    }
    if (count > 1) {
      assert.ok(
        titlePages[0].elements.some((e) => e.action?.type === "playAll"),
        `${tv} n=${count} should include playAll`,
      );
    }
    if (count === 7) {
      assert.ok(titlePages[0].elements.some((e) => e.action?.type === "menu" && e.action.menu === titlePages[1].id));
      assert.ok(titlePages[1].elements.some((e) => e.action?.type === "menu" && e.action.menu === titlePages[0].id));
    }
  }
}

// ---------------------------------------------------------------------------
// (b) Adding a video after user edits preserves moved/custom elements, zero overlaps
// ---------------------------------------------------------------------------
{
  const t1 = makeTitle(1, "/v/one.mp4", { label: "One" });
  const t2 = makeTitle(2, "/v/two.mp4", { label: "Two" });
  const oldTitles = [t1, t2];
  const layout = draftLayout(oldTitles, "PAL", "User Show");
  const main = layout.menus.find((m) => m.role === "title");

  // User customizes style, moves title 1 video, adds custom rect + blocking button on slot 2 (x=480, y=120).
  main.background.color = "#223344";
  for (const e of main.elements) {
    if (e.type === "text") {
      e.font = "Georgia";
      e.color = "#E8C872";
    }
  }
  const vid1 = main.elements.find((e) => e.type === "video" && e.title === 1);
  vid1.x = 75;
  vid1.y = 135;
  const customRect = {
    id: "custom_r1",
    type: "rect",
    x: 20,
    y: 20,
    w: 30,
    h: 10,
    rotation: 0,
    opacity: 1,
    color: "#E4002B",
    action: null,
  };
  const customBtn = {
    id: "custom_b1",
    type: "text",
    text: "Bonus",
    font: "Georgia",
    size: 20,
    color: "#E8C872",
    align: "center",
    bold: false,
    italic: false,
    shadow: true,
    x: 470,
    y: 115,
    w: 190,
    h: 140,
    rotation: 0,
    opacity: 1,
    action: { type: "playAll" },
  };
  main.elements.push(customRect, customBtn);

  const t3 = makeTitle(3, "/v/three.mp4", { label: "Three", chapters: 2 });
  const newTitles = [t1, t2, t3];
  const synced = syncLayoutWithTitles(layout, oldTitles.map((t) => t.path), newTitles, "PAL", "User Show");

  assertValidLayout(synced, newTitles, "PAL", "sync add after user edits");
  const syncedMain = synced.menus.find((m) => m.role === "title");
  assert.equal(syncedMain.background.color, "#223344");
  assert.ok(syncedMain.elements.some((e) => e.id === "custom_r1"), "custom rect preserved");
  assert.ok(syncedMain.elements.some((e) => e.id === "custom_b1"), "custom button preserved");
  const syncedVid1 = syncedMain.elements.find((e) => e.type === "video" && e.title === 1);
  assert.equal(syncedVid1.x, 75);
  assert.equal(syncedVid1.y, 135);

  // Title 3 must exist, skip blocked slot 2, and inherit user's Georgia/#E8C872 style.
  const vid3 = syncedMain.elements.find((e) => e.type === "video" && e.title === 3);
  assert.ok(vid3, "title 3 thumbnail placed");
  assert.notEqual(vid3.x, 480, "title 3 skipped blocked slot 2");
  const cap3 = syncedMain.elements.find((e) => e.type === "text" && e.action?.type === "menu" && e.text.includes("Three"));
  assert.ok(cap3, "title 3 caption button placed");
  assert.equal(cap3.font, "Georgia");
  assert.equal(cap3.color, "#E8C872");
}

// ---------------------------------------------------------------------------
// (c) Adding video #7 creates 2nd title page + Back/Next + root & settings menus
// ---------------------------------------------------------------------------
for (const tv of ["PAL", "NTSC", "BD1080"]) {
  const six = Array.from({ length: 6 }, (_, i) => makeTitle(i + 1, `/v/${i + 1}.mp4`));
  const layout6 = draftLayout(six, tv, "Six Pack");
  // Simulate a subtle user edit on page 1.
  layout6.menus[0].background.color = "#102030";
  const t7 = makeTitle(7, "/v/7.mp4", {
    label: "Seventh",
    chapters: 3,
    audioTracks: [
      { source: 1, label: "English" },
      { source: 2, label: "French" },
    ],
    subtitleTracks: [{ source: 3, label: "English Subs" }],
  });
  const seven = [...six, t7];
  const synced7 = syncLayoutWithTitles(layout6, six.map((t) => t.path), seven, tv, "Six Pack");

  assertValidLayout(synced7, seven, tv, `sync 6->7 ${tv}`);
  const titlePages = synced7.menus.filter((m) => m.role === "title");
  assert.equal(titlePages.length, 2, `${tv}: 2nd title menu page created`);
  assert.equal(titlePages[1].background.color, "#102030", `${tv}: 2nd page inherits background color`);
  const nextBtn = titlePages[0].elements.find((e) => e.action?.type === "menu" && e.action.menu === titlePages[1].id);
  const backBtn = titlePages[1].elements.find((e) => e.action?.type === "menu" && e.action.menu === titlePages[0].id);
  assert.ok(nextBtn, `${tv}: Next button on page 1`);
  assert.ok(backBtn, `${tv}: Back button on page 2`);
  assert.ok(titlePages[1].elements.some((e) => e.type === "video" && e.title === 7), `${tv}: title 7 on page 2`);

  const root7 = synced7.menus.find((m) => m.role === "root" && m.title === 7);
  const settings7 = synced7.menus.find((m) => m.role === "settings" && m.title === 7);
  assert.ok(root7, `${tv}: title 7 root menu created`);
  assert.ok(settings7, `${tv}: title 7 settings menu created`);
  assert.ok(
    root7.elements.some((e) => e.action?.type === "menu" && e.action.menu === titlePages[1].id),
    `${tv}: root7 Main menu button links back to page 2`,
  );
  assert.ok(
    settings7.elements.some((e) => e.action?.type === "menu" && e.action.menu === titlePages[1].id),
    `${tv}: settings7 Main menu button links back to page 2`,
  );
  if (tv === "BD1080") {
    const vid7 = titlePages[1].elements.find((e) => e.type === "video" && e.title === 7);
    assert.equal(vid7.w, 400);
    assert.equal(vid7.h, 225);
  }
}

// Enlarged/moved heading overlapping slot rows on page 1 must clamp to default heading box on page 2.
{
  const six = Array.from({ length: 6 }, (_, i) => makeTitle(i + 1, `/v/${i + 1}.mp4`));
  const layout6 = draftLayout(six, "PAL", "Big Heading");
  const head1 = layout6.menus[0].elements.find((e) => e.type === "text" && !e.action);
  head1.font = "Georgia";
  head1.color = "#E8C872";
  head1.y = 30;
  head1.h = 400; // extends deep into the 3x2 grid
  const seven = [...six, makeTitle(7, "/v/7.mp4", { label: "Seventh" })];
  const synced7 = syncLayoutWithTitles(layout6, six.map((t) => t.path), seven, "PAL", "Big Heading");
  assertValidLayout(synced7, seven, "PAL", "enlarged heading pagination");
  const page2 = synced7.menus.filter((m) => m.role === "title")[1];
  const head2 = page2.elements.find((e) => e.type === "text" && !e.action && e.text === "Big Heading");
  assert.ok(head2, "page 2 heading created");
  assert.equal(head2.font, "Georgia");
  assert.equal(head2.color, "#E8C872");
  assert.ok(head2.y + head2.h <= 112, "page 2 heading clamped above slot grid");
  assert.ok(page2.elements.some((e) => e.type === "video" && e.title === 7), "title 7 placed on page 2");
}

// ---------------------------------------------------------------------------
// (d) Removing a video cleans up VTSM menus, buttons, captions & dangling links
// ---------------------------------------------------------------------------
{
  const t1 = makeTitle(1, "/v/first.mp4", {
    label: "First",
    chapters: 3,
    subtitleTracks: [{ source: 2, label: "EN" }],
  });
  const t2 = makeTitle(2, "/v/second.mp4", { label: "Second", chapters: 2 });
  const layout = draftLayout([t1, t2], "PAL", "Removals");
  const main = layout.menus.find((m) => m.role === "title");
  // Customize title 2's thumbnail & caption position so we can verify it survives remapping from 2 -> 1.
  const vid2 = main.elements.find((e) => e.type === "video" && e.title === 2);
  const cap2 = main.elements.find((e) => e.type === "text" && e.text.includes("Second"));
  vid2.x = 310;
  vid2.y = 145;
  cap2.x = 310;
  cap2.y = 263;
  // Also add an extra custom button linking to title 1's root menu (should be cleaned up).
  const root1 = layout.menus.find((m) => m.role === "root" && m.title === 1);
  main.elements.push({
    id: "dangling_user_btn",
    type: "text",
    text: "Jump to First",
    font: "Arial",
    size: 18,
    color: "#FFFFFF",
    align: "center",
    bold: false,
    italic: false,
    shadow: true,
    x: 60,
    y: 440,
    w: 180,
    h: 30,
    rotation: 0,
    opacity: 1,
    action: { type: "menu", menu: root1.id },
  });

  const surviving = [makeTitle(1, "/v/second.mp4", { label: "Second", chapters: 2 })];
  const synced = syncLayoutWithTitles(layout, [t1.path, t2.path], surviving, "PAL", "Removals");

  assertValidLayout(synced, surviving, "PAL", "sync removal");
  assert.ok(!synced.menus.some((m) => m.id === root1.id), "removed title's root menu deleted");
  assert.ok(!synced.menus.some((m) => m.role === "settings"), "removed title's settings menu deleted");
  const syncedMain = synced.menus.find((m) => m.role === "title");
  assert.ok(!syncedMain.elements.some((e) => e.id === "dangling_user_btn"), "dangling menu link removed");
  assert.equal(
    syncedMain.elements.filter((e) => e.type === "video").length,
    1,
    "only surviving video thumbnail remains",
  );
  const remappedVid = syncedMain.elements.find((e) => e.type === "video" && e.title === 1);
  assert.equal(remappedVid.x, 310, "surviving title 2 remapped to 1 with custom position preserved");
  assert.equal(remappedVid.y, 145);
}

// ---------------------------------------------------------------------------
// (e) Duplicate paths (1-to-1) & legacy v1 project layoutEdited default
// ---------------------------------------------------------------------------
{
  const dupOld = [makeTitle(1, "/v/ep.mp4", { label: "Ep Part 1" }), makeTitle(2, "/v/ep.mp4", { label: "Ep Part 2" })];
  const dupLayout = draftLayout(dupOld, "PAL", "Dupes");
  const dupNew = [
    makeTitle(1, "/v/intro.mp4", { label: "Intro" }),
    makeTitle(2, "/v/ep.mp4", { label: "Ep Part 1" }),
    makeTitle(3, "/v/ep.mp4", { label: "Ep Part 2" }),
  ];
  const syncedDup = syncLayoutWithTitles(dupLayout, dupOld.map((t) => t.path), dupNew, "PAL", "Dupes");
  assertValidLayout(syncedDup, dupNew, "PAL", "duplicate paths prepend");
  const playTitles = syncedDup.menus[0].elements
    .filter((e) => e.action?.type === "playTitle")
    .map((e) => e.action.title)
    .sort((a, b) => a - b);
  assert.deepEqual(playTitles, [1, 2, 3], "each duplicate keeps distinct title index + new title added");

  // Removing one duplicate drops only the second copy cleanly.
  const oneLeft = [makeTitle(1, "/v/ep.mp4", { label: "Ep Part 1" })];
  const syncedDrop = syncLayoutWithTitles(dupLayout, dupOld.map((t) => t.path), oneLeft, "PAL", "Dupes");
  assertValidLayout(syncedDrop, oneLeft, "PAL", "duplicate paths drop one");
  assert.equal(
    syncedDrop.menus[0].elements.filter((e) => e.action?.type === "playTitle").length,
    1,
    "only one duplicate button remains",
  );

  // Legacy v1 .musadvd project without layoutEdited field:
  const sampleLayout = draftLayout([makeTitle(1, "/v/a.mp4")], "PAL", "Legacy");
  const baseProject = {
    format: PROJECT_FORMAT,
    version: PROJECT_VERSION,
    savedAt: "2026-01-01T00:00:00.000Z",
    disc: "dvd5",
    tvStandard: "PAL",
    twoPass: false,
    motion: false,
    bdVideo: "encode",
    bdAudio: "auto",
    template: "",
    menuMode: "custom",
    menuModeBd: "custom",
    customLayout: sampleLayout,
    customLayoutBd: null,
    layoutKey: titlesKey([makeTitle(1, "/v/a.mp4")]),
    layoutKeyBd: null,
    menuTitle: "Legacy",
    menuTitleAuto: false,
    output: "C:/Out",
    titles: [],
  };
  const parsedLegacy = parseProject(baseProject);
  assert.equal(parsedLegacy.layoutEdited, true, "legacy v1 with non-null customLayout defaults layoutEdited = true");
  assert.equal(parsedLegacy.layoutEditedBd, false, "legacy v1 with null customLayoutBd defaults layoutEditedBd = false");

  const parsedExplicitFalse = parseProject({ ...baseProject, layoutEdited: false });
  assert.equal(parsedExplicitFalse.layoutEdited, false, "explicit layoutEdited: false is preserved");
}

console.log("check-layout-sync: ok");
