import { expect, test, type Page } from "@playwright/test";

declare global {
  interface Window {
    annotationTest: {
      deferNext: boolean;
      rejectAdvance: boolean;
      deferScroll: boolean;
      rejectScroll: boolean;
      rejectImage: boolean;
      deferFreeze: boolean;
      freezes: { sessionId: number; captureId: number }[];
      scrolls: {
        sessionId: number;
        captureId: number;
        deltaX: number;
        deltaY: number;
      }[];
      sourceScrollTop: () => number;
      advances: { sessionId: number; pointNumber: number }[];
      submissions: {
        sessionId: number;
        pointCount: number;
        pngDataUrl: string;
        additionalPngDataUrls: string[];
      }[];
      resolveNext?: () => void;
      resolveScroll?: () => void;
      resolveFreeze?: () => void;
      emit: (event: string, payload?: object) => Promise<void>;
    };
  }
}

test.beforeEach(async ({ page }) => {
  await page.route("**/src/annotation/index.html*", async (route) => {
    const response = await route.fetch();
    const setup = `<script type="module">
      import { mockIPC } from "/node_modules/@tauri-apps/api/mocks.js";
      import { emit } from "/node_modules/@tauri-apps/api/event.js";
      const state = window.annotationTest = {
        deferNext: false, rejectAdvance: false, advances: [], submissions: [],
        deferScroll: false, rejectScroll: false, rejectImage: false, scrolls: [], images: new Map(),
        deferFreeze: false, freezes: [],
        snapshot: { session_id: 42, capture_id: 0, path: "synthetic.png", width: 2560, height: 1440, point_count: 1 },
        emit: (name, payload) => {
          if (name === "reset-screen-annotation") state.snapshot = payload;
          if (name === "annotation-point-changed" && payload.session_id === state.snapshot.session_id)
            state.snapshot = { ...state.snapshot, point_count: payload.point_count };
          return emit(name, payload);
        },
      };
      const screenshot = document.createElement("canvas");
      screenshot.width = 2560;
      screenshot.height = 1440;
      const context = screenshot.getContext("2d");
      context.fillStyle = "#fff";
      context.fillRect(0, 0, screenshot.width, screenshot.height);
      state.images.set("synthetic.png", screenshot.toDataURL());
      const sourceFrame = document.createElement("iframe");
      sourceFrame.style.cssText = "position:fixed;left:-10000px;width:2560px;height:1440px;border:0";
      document.body.append(sourceFrame);
      const source = sourceFrame.contentWindow;
      source.document.write('<!doctype html><body style="margin:0"><div style="height:1440px;background:white"></div><div style="height:1440px;background:#dbeafe"></div><div style="height:1440px;background:#dcfce7"></div></body>');
      source.document.close();
      state.sourceScrollTop = () => source.document.scrollingElement.scrollTop;
      const scrollSource = (args) => {
        if (state.snapshot.session_id !== args.sessionId) throw new Error("Session cancelled");
        source.scrollBy(args.deltaX, -args.deltaY);
        return state.snapshot;
      };
      const captureSource = (args) => {
        if (state.snapshot.session_id !== args.sessionId) throw new Error("Session cancelled");
        const offset = state.sourceScrollTop();
        const path = "synthetic-" + offset + ".png";
        ["#fff", "#dbeafe", "#dcfce7"].forEach((color, index) => {
          context.fillStyle = color;
          context.fillRect(0, index * 1440 - offset, 2560, 1440);
        });
        const pixels = screenshot.toDataURL();
        const changed = pixels !== state.images.get(state.snapshot.path);
        state.images.set(path, pixels);
        state.snapshot = {
          ...state.snapshot, path: changed ? path : state.snapshot.path,
          capture_id: state.snapshot.capture_id + (changed ? 1 : 0),
        };
        return state.rejectImage ? { ...state.snapshot, path: "broken.png" } : state.snapshot;
      };
      mockIPC((command, args) => {
        switch (command) {
          case "get_app_settings": return { app_language: "en" };
          case "get_screen_annotation_snapshot":
            return state.snapshot;
          case "show_screen_annotation": return;
          case "advance_screen_annotation":
            state.advances.push(args);
            if (state.rejectAdvance) throw new Error("Point transition failed");
            if (state.deferNext) return new Promise(resolve => {
              state.resolveNext = () => resolve(args.pointNumber + 1);
            });
            return args.pointNumber + 1;
          case "scroll_screen_annotation":
            state.scrolls.push(args);
            if (state.rejectScroll) throw new Error("Source scroll failed");
            if (state.deferScroll) return new Promise((resolve, reject) => {
              state.resolveScroll = () => {
                try { resolve(scrollSource(args)); } catch (error) { reject(error); }
              };
            });
            return scrollSource(args);
          case "freeze_screen_annotation":
            state.freezes.push(args);
            if (state.deferFreeze) return new Promise((resolve, reject) => {
              state.resolveFreeze = () => {
                try { resolve(captureSource(args)); } catch (error) { reject(error); }
              };
            });
            return captureSource(args);
          case "complete_screen_annotation": return emit("finish-screen-annotation");
          case "submit_screen_annotation": state.submissions.push(args); return;
          default: throw new Error("Unexpected test command: " + command);
        }
      }, { shouldMockEvents: true });
      window.__TAURI_INTERNALS__.convertFileSrc = (path) =>
        new URLSearchParams(location.search).has("delayImage")
          ? "/annotation-test-image.png" : state.images.get(path);
    </script>`;
    await route.fulfill({
      response,
      body: (await response.text()).replace("</head>", `${setup}</head>`),
    });
  });
  await page.goto("/src/annotation/index.html");
  await expect(page.locator("canvas")).toHaveJSProperty("width", 2560);
});

async function stroke(page: Page, x: number, y: number) {
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + 50, y + 30, { steps: 4 });
  await page.mouse.up();
}

test("groups strokes, preserves numbers when clearing, and exports scaled badges", async ({
  page,
}) => {
  const next = page.getByRole("button", { name: "Next point" });
  const markers = page.locator(".annotation-marker");
  await expect(next).toBeDisabled();
  await stroke(page, 220, 200);
  await stroke(page, 300, 220);
  await expect(markers).toHaveCount(1);
  await next.click();
  await expect(markers).toHaveCount(2);
  await stroke(page, 600, 250);
  await page.getByRole("button", { name: "Clear ink" }).click();
  await expect(markers).toHaveCount(2);
  await page.locator(".annotation-done").click();
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  const output = await page.evaluate(async () => {
    const submission = window.annotationTest.submissions[0];
    const image = new Image();
    image.src = submission.pngDataUrl;
    await image.decode();
    const canvas = document.createElement("canvas");
    canvas.width = image.width;
    canvas.height = image.height;
    const context = canvas.getContext("2d")!;
    context.drawImage(image, 0, 0);
    return {
      pointCount: submission.pointCount,
      firstBadge: Array.from(context.getImageData(454, 414, 1, 1).data),
      secondBadge: Array.from(context.getImageData(1214, 514, 1, 1).data),
      clearedInk: Array.from(context.getImageData(500, 436, 1, 1).data),
    };
  });
  expect(output).toEqual({
    pointCount: 2,
    firstBadge: [17, 17, 17, 255],
    secondBadge: [17, 17, 17, 255],
    clearedInk: [255, 255, 255, 255],
  });
});

test("finishing waits for a pending point boundary", async ({ page }) => {
  await stroke(page, 220, 200);
  await page.evaluate(() => {
    window.annotationTest.deferNext = true;
  });
  await page.getByRole("button", { name: "Next point" }).click();
  await page.evaluate(() =>
    window.annotationTest.emit("finish-screen-annotation"),
  );
  expect(
    await page.evaluate(() => window.annotationTest.submissions.length),
  ).toBe(0);
  await page.evaluate(() => window.annotationTest.resolveNext?.());
  await expect
    .poll(() =>
      page.evaluate(() => window.annotationTest.submissions[0]?.pointCount),
    )
    .toBe(2);
  expect(await page.evaluate(() => window.annotationTest.advances)).toEqual([
    { sessionId: 42, pointNumber: 1 },
  ]);
});

test("ignores an old session response and exposes point errors", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.evaluate(() => {
    window.annotationTest.deferNext = true;
  });
  const next = page.getByRole("button", { name: "Next point" });
  await next.click();
  await page.evaluate(() =>
    window.annotationTest.emit("finish-screen-annotation"),
  );
  await page.evaluate(() =>
    window.annotationTest.emit("reset-screen-annotation", {
      session_id: 43,
      capture_id: 0,
      path: "synthetic.png",
      width: 2560,
      height: 1440,
      point_count: 1,
    }),
  );
  await expect(next).toBeDisabled();
  await page.evaluate(() => {
    window.annotationTest.resolveNext?.();
    window.annotationTest.deferNext = false;
    window.annotationTest.rejectAdvance = true;
  });

  await expect(page.locator(".annotation-marker")).toHaveCount(1);
  expect(
    await page.evaluate(() => window.annotationTest.submissions.length),
  ).toBe(0);
  await stroke(page, 600, 250);
  await next.click();
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.locator(".annotation-marker")).toHaveCount(1);
  await page.evaluate(() => {
    window.annotationTest.rejectAdvance = false;
  });
  await next.click();
  await expect(page.locator(".annotation-marker")).toHaveCount(2);
  expect(
    await page.evaluate(() => window.annotationTest.advances.slice(-1)),
  ).toEqual([{ sessionId: 43, pointNumber: 1 }]);
});

test("keyboard point updates do not duplicate boundaries or continue an old stroke", async ({
  page,
}) => {
  await page.mouse.move(220, 200);
  await page.mouse.down();
  await page.mouse.move(250, 220);
  await page.evaluate(async () => {
    const snapshot = {
      session_id: 42,
      capture_id: 0,
      path: "synthetic.png",
      width: 2560,
      height: 1440,
    };
    for (const point_count of [2, 3, 3]) {
      await window.annotationTest.emit("annotation-point-changed", {
        ...snapshot,
        point_count,
      });
    }
    await window.annotationTest.emit("annotation-point-changed", {
      ...snapshot,
      session_id: 41,
      point_count: 4,
    });
  });
  await expect(page.locator(".annotation-marker")).toHaveCount(3);
  await page.mouse.move(600, 250);
  await page.mouse.up();
  const pixel = await page
    .locator("canvas")
    .evaluate((canvas: HTMLCanvasElement) =>
      Array.from(canvas.getContext("2d")!.getImageData(1200, 500, 1, 1).data),
    );
  expect(pixel).toEqual([255, 255, 255, 255]);
  expect(await page.evaluate(() => window.annotationTest.advances)).toEqual([]);
  await stroke(page, 600, 250);
  await page.locator(".annotation-done").click();
  await expect
    .poll(() =>
      page.evaluate(() => window.annotationTest.submissions[0]?.pointCount),
    )
    .toBe(3);
});

test("a quick hold release waits for the captured image before exporting", async ({
  page,
}) => {
  let releaseImage = () => {};
  const imageReady = new Promise<void>((resolve) => {
    releaseImage = resolve;
  });
  await page.route("**/annotation-test-image.png", async (route) => {
    await imageReady;
    await route.fulfill({
      contentType: "image/svg+xml",
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="2560" height="1440"><rect width="100%" height="100%" fill="white"/></svg>',
    });
  });
  await page.goto("/src/annotation/index.html?delayImage=1", {
    waitUntil: "domcontentloaded",
  });
  await expect(page.locator(".annotation-marker")).toHaveCount(1);
  await page.evaluate(() =>
    window.annotationTest.emit("finish-screen-annotation"),
  );
  expect(
    await page.evaluate(() => window.annotationTest.submissions.length),
  ).toBe(0);
  releaseImage();
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  const dimensions = await page.evaluate(async () => {
    const image = new Image();
    image.src = window.annotationTest.submissions[0].pngDataUrl;
    await image.decode();
    return [image.width, image.height];
  });
  expect(dimensions).toEqual([2560, 1440]);
});

test("scrolls a synthetic source page and preserves old ink on its own exported screen", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.getByRole("button", { name: "Next point" }).click();
  await page.mouse.move(600, 400);
  await page.mouse.wheel(0, 1440);
  await expect(page.locator("main")).toHaveAttribute("aria-busy", "false");
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.sourceScrollTop()))
    .toBe(1440);
  await expect(page.locator("main")).toHaveClass(/annotation-browsing/);
  await stroke(page, 600, 250);
  await expect(page.locator("main")).not.toHaveClass(/annotation-browsing/);
  await expect(page.locator(".annotation-marker")).toHaveCount(1);
  await expect(page.locator(".annotation-marker")).toHaveText("2");
  await expect
    .poll(() =>
      page
        .locator("canvas")
        .evaluate((canvas: HTMLCanvasElement) =>
          Array.from(
            canvas.getContext("2d")!.getImageData(500, 436, 1, 1).data,
          ),
        ),
    )
    .toEqual([219, 234, 254, 255]);
  await stroke(page, 600, 250);
  await page.getByRole("button", { name: "Clear ink" }).click();
  await page.locator(".annotation-done").click();
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  const result = await page.evaluate(async () => {
    const submission = window.annotationTest.submissions[0];
    const pngs = [submission.pngDataUrl, ...submission.additionalPngDataUrls];
    const screens = await Promise.all(
      pngs.map(async (png) => {
        const image = new Image();
        image.src = png;
        await image.decode();
        const canvas = document.createElement("canvas");
        canvas.width = image.width;
        canvas.height = image.height;
        const context = canvas.getContext("2d")!;
        context.drawImage(image, 0, 0);
        return {
          size: [image.width, image.height],
          oldInk: Array.from(context.getImageData(500, 516, 1, 1).data),
        };
      }),
    );
    return {
      pointCount: submission.pointCount,
      screens,
      scrolls: window.annotationTest.scrolls,
    };
  });
  expect(result.pointCount).toBe(2);
  expect(result.screens).toEqual([
    { size: [2560, 1520], oldInk: [255, 45, 85, 255] },
    { size: [2560, 1520], oldInk: [219, 234, 254, 255] },
  ]);
  expect(result.scrolls).toEqual([
    {
      sessionId: 42,
      captureId: 0,
      deltaX: 0,
      deltaY: -1440,
      x: 600 / 1280,
      y: 400 / 720,
    },
  ]);
});

test("browses without recapture and freezes on first press without losing its drag", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.evaluate(() => {
    window.annotationTest.deferScroll = true;
  });
  const canvas = page.locator("canvas");
  await canvas.dispatchEvent("wheel", {
    clientX: 600,
    clientY: 400,
    deltaY: 1440,
  });
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.scrolls.length))
    .toBe(1);
  await canvas.dispatchEvent("wheel", {
    clientX: 600,
    clientY: 400,
    deltaY: 1440,
  });
  await page.evaluate(() => window.annotationTest.resolveScroll?.());
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.scrolls.length))
    .toBe(2);
  await expect(page.locator("main")).toHaveClass(/annotation-browsing/);
  expect(await page.evaluate(() => window.annotationTest.freezes)).toEqual([]);
  await expect
    .poll(() =>
      canvas.evaluate((element: HTMLCanvasElement) =>
        Array.from(element.getContext("2d")!.getImageData(500, 436, 1, 1).data),
      ),
    )
    .toEqual([0, 0, 0, 0]);
  await page.mouse.move(600, 250);
  await page.mouse.down();
  await page.mouse.move(650, 280, { steps: 4 });
  await page.mouse.up();
  await page.evaluate(() => window.annotationTest.resolveScroll?.());
  await expect(page.locator("main")).toHaveAttribute("aria-busy", "false");
  await expect
    .poll(() =>
      canvas.evaluate((element: HTMLCanvasElement) =>
        Array.from(element.getContext("2d")!.getImageData(500, 436, 1, 1).data),
      ),
    )
    .toEqual([220, 252, 231, 255]);
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.freezes.length))
    .toBe(1);
  await expect
    .poll(() =>
      canvas.evaluate((element: HTMLCanvasElement) =>
        Array.from(
          element.getContext("2d")!.getImageData(1250, 530, 1, 1).data,
        ),
      ),
    )
    .toEqual([255, 45, 85, 255]);
  await page.evaluate(() =>
    window.annotationTest.emit("finish-screen-annotation"),
  );
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  const result = await page.evaluate(async () => {
    const submission = window.annotationTest.submissions[0];
    const image = new Image();
    image.src = submission.pngDataUrl;
    await image.decode();
    const output = document.createElement("canvas");
    output.width = image.width;
    output.height = image.height;
    const context = output.getContext("2d")!;
    context.drawImage(image, 0, 0);
    return {
      extraScreens: submission.additionalPngDataUrls.length,
      originalInk: Array.from(context.getImageData(500, 516, 1, 1).data),
    };
  });
  expect(result).toEqual({ extraScreens: 1, originalInk: [255, 45, 85, 255] });
});

test("native browse input buffers the intercepted gesture across capture and ignores stale sessions", async ({
  page,
}) => {
  const canvas = page.locator("canvas");
  await canvas.dispatchEvent("wheel", {
    clientX: 600,
    clientY: 400,
    deltaY: 1440,
  });
  await expect(page.locator("main")).toHaveAttribute("aria-busy", "false");
  await expect(page.locator("main")).toHaveClass(/annotation-browsing/);
  await page.evaluate(async () => {
    const state = window.annotationTest;
    await state.emit("annotation-browse-input", {
      session_id: 42,
      kind: "wheel",
      x: 0.5,
      y: 0.5,
      delta_x: 0,
      delta_y: -1440,
    });
  });
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.sourceScrollTop()))
    .toBe(2880);
  await page.evaluate(async () => {
    const state = window.annotationTest;
    state.deferFreeze = true;
    for (const [kind, x, y] of [
      ["down", 0.5, 0.5],
      ["move", 0.55, 0.55],
      ["up", 0.55, 0.55],
    ])
      await state.emit("annotation-browse-input", {
        session_id: 42,
        kind,
        x,
        y,
        delta_x: 0,
        delta_y: 0,
      });
  });
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.freezes.length))
    .toBe(1);
  await page.evaluate(() => window.annotationTest.resolveFreeze?.());
  await expect
    .poll(() =>
      canvas.evaluate((element: HTMLCanvasElement) =>
        Array.from(
          element.getContext("2d")!.getImageData(1344, 756, 1, 1).data,
        ),
      ),
    )
    .toEqual([255, 45, 85, 255]);
  await expect(page.locator("main")).not.toHaveClass(/annotation-browsing/);
  await page.evaluate(async () => {
    await window.annotationTest.emit("annotation-browse-input", {
      session_id: 41,
      kind: "wheel",
      x: 0.5,
      y: 0.5,
      delta_x: 0,
      delta_y: -1440,
    });
    await window.annotationTest.emit("finish-screen-annotation");
  });
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  expect(
    await page.evaluate(() => ({
      scrolls: window.annotationTest.scrolls.length,
      pages: window.annotationTest.submissions[0].additionalPngDataUrls.length,
    })),
  ).toEqual({ scrolls: 2, pages: 1 });
});

test("release waits for in-flight capture, keeps Ctrl numbering and cancels queued future scrolls", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.evaluate(() => {
    window.annotationTest.deferScroll = true;
  });
  await page.mouse.move(600, 400);
  await page.mouse.wheel(0, 1440);
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.scrolls.length))
    .toBe(1);
  await page.mouse.wheel(0, 900);
  await page.evaluate(async () => {
    await window.annotationTest.emit("annotation-point-changed", {
      session_id: 42,
      capture_id: 0,
      path: "synthetic.png",
      width: 2560,
      height: 1440,
      point_count: 2,
    });
    await window.annotationTest.emit("finish-screen-annotation");
  });
  expect(
    await page.evaluate(() => window.annotationTest.submissions.length),
  ).toBe(0);
  await page.evaluate(() => window.annotationTest.resolveScroll?.());
  await expect
    .poll(() =>
      page.evaluate(() => window.annotationTest.submissions[0]?.pointCount),
    )
    .toBe(2);
  expect(
    await page.evaluate(
      () => window.annotationTest.submissions[0].additionalPngDataUrls.length,
    ),
  ).toBe(1);
  expect(await page.evaluate(() => window.annotationTest.scrolls.length)).toBe(
    1,
  );
});

test("release during first-press capture preserves the buffered stroke and its pre-Ctrl number", async ({
  page,
}) => {
  await page.evaluate(() => {
    window.annotationTest.deferFreeze = true;
  });
  await page.mouse.move(600, 250);
  await page.mouse.wheel(0, 1440);
  await expect(page.locator("main")).toHaveClass(/annotation-browsing/);
  await page.mouse.down();
  await page.mouse.move(650, 280, { steps: 4 });
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.freezes.length))
    .toBe(1);
  await page.evaluate(() =>
    window.annotationTest.emit("annotation-point-changed", {
      session_id: 42,
      capture_id: 0,
      path: "synthetic.png",
      width: 2560,
      height: 1440,
      point_count: 2,
    }),
  );
  await page.mouse.move(700, 350, { steps: 4 });
  await page.mouse.up();
  await page.evaluate(() =>
    window.annotationTest.emit("finish-screen-annotation"),
  );
  expect(
    await page.evaluate(() => window.annotationTest.submissions.length),
  ).toBe(0);
  await page.evaluate(() => window.annotationTest.resolveFreeze?.());
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  await expect(page.locator(".annotation-marker")).toHaveText(["1", "2"]);
  await expect(page.locator(".annotation-marker").first()).toHaveCSS(
    "left",
    "600px",
  );
  const result = await page.evaluate(async () => {
    const submission = window.annotationTest.submissions[0];
    const image = new Image();
    image.src = submission.additionalPngDataUrls[0];
    await image.decode();
    const canvas = document.createElement("canvas");
    canvas.width = image.width;
    canvas.height = image.height;
    const context = canvas.getContext("2d")!;
    context.drawImage(image, 0, 0);
    return {
      count: submission.pointCount,
      firstDrag: Array.from(context.getImageData(1250, 610, 1, 1).data),
      afterCtrl: Array.from(context.getImageData(1350, 710, 1, 1).data),
    };
  });
  expect(result).toEqual({
    count: 2,
    firstDrag: [255, 45, 85, 255],
    afterCtrl: [219, 234, 254, 255],
  });
});

test("Ctrl advances made while browsing retain even undrawn numbers on the new screen", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.mouse.move(600, 400);
  await page.mouse.wheel(0, 1440);
  await expect(page.locator("main")).toHaveClass(/annotation-browsing/);
  await page.evaluate(async () => {
    for (const count of [2, 3]) {
      await window.annotationTest.emit("annotation-point-changed", {
        session_id: 42,
        capture_id: 0,
        path: "synthetic.png",
        width: 2560,
        height: 1440,
        point_count: count,
      });
    }
    await window.annotationTest.emit("finish-screen-annotation");
  });
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  await expect(page.locator(".annotation-marker")).toHaveText(["2", "3"]);
  expect(
    await page.evaluate(() => window.annotationTest.submissions[0].pointCount),
  ).toBe(3);
});

test("a stale first-press capture cannot draw on or submit the next session", async ({
  page,
}) => {
  await page.evaluate(() => {
    window.annotationTest.deferFreeze = true;
  });
  await page.mouse.move(600, 250);
  await page.mouse.wheel(0, 1440);
  await expect(page.locator("main")).toHaveClass(/annotation-browsing/);
  await stroke(page, 600, 250);
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.freezes.length))
    .toBe(1);
  await page.evaluate(async () => {
    await window.annotationTest.emit("reset-screen-annotation", {
      session_id: 43,
      capture_id: 0,
      path: "synthetic.png",
      width: 2560,
      height: 1440,
      point_count: 1,
    });
    window.annotationTest.resolveFreeze?.();
  });
  await expect(page.locator("main")).not.toHaveClass(/annotation-browsing/);
  await expect(page.getByRole("alert")).toHaveCount(0);
  await page.locator(".annotation-done").click();
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  expect(
    await page.evaluate(() => ({
      session: window.annotationTest.submissions[0].sessionId,
      screens:
        window.annotationTest.submissions[0].additionalPngDataUrls.length,
    })),
  ).toEqual({ session: 43, screens: 0 });
});

test("a failed scroll leaves all current ink intact and permits an explicit retry", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.evaluate(() => {
    window.annotationTest.rejectScroll = true;
  });
  await page.mouse.move(600, 400);
  await page.mouse.wheel(0, 1440);
  await expect(page.getByRole("alert")).toContainText("Scroll to retry");
  expect(
    await page
      .locator("canvas")
      .evaluate((canvas: HTMLCanvasElement) =>
        Array.from(canvas.getContext("2d")!.getImageData(500, 436, 1, 1).data),
      ),
  ).toEqual([255, 45, 85, 255]);
  await page.evaluate(() => {
    window.annotationTest.rejectScroll = false;
  });
  await page.mouse.wheel(0, 1440);
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.sourceScrollTop()))
    .toBe(1440);
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(page.locator(".annotation-done")).toBeEnabled();
  await page.evaluate(() =>
    window.annotationTest.emit("finish-screen-annotation"),
  );
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          window.annotationTest.submissions[0]?.additionalPngDataUrls.length,
      ),
    )
    .toBe(1);
});

test("a stale scroll response cannot replace or submit a newer annotation session", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.evaluate(() => {
    window.annotationTest.deferScroll = true;
  });
  await page.mouse.move(600, 400);
  await page.mouse.wheel(0, 1440);
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.scrolls.length))
    .toBe(1);
  await page.evaluate(async () => {
    await window.annotationTest.emit("reset-screen-annotation", {
      session_id: 43,
      capture_id: 0,
      path: "synthetic.png",
      width: 2560,
      height: 1440,
      point_count: 1,
    });
    window.annotationTest.resolveScroll?.();
  });
  await expect(page.locator(".annotation-marker")).toHaveCount(1);
  await expect(page.getByRole("alert")).toHaveCount(0);
  await page.locator(".annotation-done").click();
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  expect(
    await page.evaluate(() => ({
      sessionId: window.annotationTest.submissions[0].sessionId,
      pages: window.annotationTest.submissions[0].additionalPngDataUrls.length,
    })),
  ).toEqual({ sessionId: 43, pages: 0 });
});

test("retries a failed image decode even when the source has reached the end of the page", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.evaluate(() => {
    window.annotationTest.rejectImage = true;
  });
  await page.mouse.move(600, 400);
  await page.mouse.wheel(0, 2880);
  await page.mouse.move(600, 400);
  await page.mouse.down();
  await page.mouse.up();
  await expect(page.getByRole("alert")).toContainText("Scroll to retry");
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.sourceScrollTop()))
    .toBe(2880);
  await page.evaluate(() => {
    window.annotationTest.rejectImage = false;
  });
  await page.mouse.wheel(0, 120);
  await stroke(page, 600, 400);
  await expect
    .poll(() =>
      page
        .locator("canvas")
        .evaluate((canvas: HTMLCanvasElement) =>
          Array.from(
            canvas.getContext("2d")!.getImageData(500, 436, 1, 1).data,
          ),
        ),
    )
    .toEqual([220, 252, 231, 255]);
  await page.locator(".annotation-done").click();
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          window.annotationTest.submissions[0]?.additionalPngDataUrls.length,
      ),
    )
    .toBe(1);
});

test("scrolling against the top of a page does not erase ink or add a duplicate screen", async ({
  page,
}) => {
  await stroke(page, 220, 200);
  await page.mouse.move(600, 400);
  await page.mouse.wheel(0, -120);
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.scrolls.length))
    .toBe(1);
  await expect(page.locator("main")).toHaveAttribute("aria-busy", "false");
  await page.evaluate(() =>
    window.annotationTest.emit("finish-screen-annotation"),
  );
  await expect
    .poll(() => page.evaluate(() => window.annotationTest.submissions.length))
    .toBe(1);
  expect(
    await page
      .locator("canvas")
      .evaluate((canvas: HTMLCanvasElement) =>
        Array.from(canvas.getContext("2d")!.getImageData(500, 436, 1, 1).data),
      ),
  ).toEqual([255, 45, 85, 255]);
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          window.annotationTest.submissions[0]?.additionalPngDataUrls.length,
      ),
    )
    .toBe(0);
});
