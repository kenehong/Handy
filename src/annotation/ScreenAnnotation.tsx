import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { ArrowRight, Check, Trash2 } from "lucide-react";
import React, { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { events } from "@/bindings";
import type { ScreenAnnotationSnapshot, StreamTextEvent } from "@/bindings";

interface Point {
  x: number;
  y: number;
}

interface AnnotationPoint {
  number: number;
  position?: Point;
}

interface CapturedPage {
  canvas: HTMLCanvasElement;
  points: AnnotationPoint[];
  scale: number;
}

interface PageScroll {
  x: number;
  y: number;
  deltaX: number;
  deltaY: number;
}

interface BrowseInput {
  session_id: number;
  kind: "down" | "move" | "up" | "wheel";
  x: number;
  y: number;
  delta_x: number;
  delta_y: number;
}

function markerPosition(
  point: AnnotationPoint,
  width: number,
  height: number,
  scale: number,
): Point {
  const margin = 18 * scale;
  const rows = Math.max(1, Math.floor((height - margin * 2) / (36 * scale)));
  const position = point.position ?? {
    x: margin + Math.floor((point.number - 1) / rows) * 36 * scale,
    y: margin + ((point.number - 1) % rows) * 36 * scale,
  };
  return {
    x: Math.max(margin, Math.min(width - margin, position.x)),
    y: Math.max(margin, Math.min(height - margin, position.y)),
  };
}

function preservePage(
  canvas: HTMLCanvasElement,
  points: AnnotationPoint[],
): CapturedPage {
  const copy = document.createElement("canvas");
  copy.width = canvas.width;
  copy.height = canvas.height;
  const context = copy.getContext("2d");
  if (!context) throw new Error("Cannot preserve the annotated screen");
  context.drawImage(canvas, 0, 0);
  return {
    canvas: copy,
    points: [...points],
    scale: canvas.width / canvas.getBoundingClientRect().width,
  };
}

function renderPage(page: CapturedPage, heading?: string): string {
  const { canvas, points, scale } = page;
  const header = heading ? Math.ceil(40 * scale) : 0;
  const output = document.createElement("canvas");
  output.width = canvas.width;
  output.height = canvas.height + header;
  const context = output.getContext("2d");
  if (!context) throw new Error("Cannot render the numbered screenshot");
  if (heading) {
    context.fillStyle = "#111";
    context.fillRect(0, 0, output.width, header);
    context.fillStyle = "#fff";
    context.font = `${16 * scale}px sans-serif`;
    context.textBaseline = "middle";
    context.fillText(heading, 18 * scale, header / 2);
  }
  context.translate(0, header);
  context.drawImage(canvas, 0, 0);
  for (const point of points) {
    const position = markerPosition(point, canvas.width, canvas.height, scale);
    context.beginPath();
    context.arc(position.x, position.y, 14 * scale, 0, Math.PI * 2);
    context.fillStyle = "#111";
    context.fill();
    context.lineWidth = 2 * scale;
    context.strokeStyle = "#fff";
    context.stroke();
    context.font = `bold ${14 * scale}px sans-serif`;
    context.textAlign = "center";
    context.textBaseline = "middle";
    context.fillStyle = "#fff";
    context.fillText(String(point.number), position.x, position.y);
  }
  const png = output.toDataURL("image/png");
  if (!png.startsWith("data:image/png;base64,"))
    throw new Error("The annotated screen is too large to export");
  return png;
}

const ScreenAnnotation: React.FC = () => {
  const { t } = useTranslation();
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const snapshotImageRef = useRef<HTMLImageElement>();
  const snapshotReadyRef = useRef<Promise<void>>();
  const drawingRef = useRef(false);
  const lastPointRef = useRef<Point>();
  const sessionIdRef = useRef<number>();
  const captureIdRef = useRef(0);
  const displayedCaptureIdRef = useRef(0);
  const pointCountRef = useRef(1);
  const pointsRef = useRef<AnnotationPoint[]>([{ number: 1 }]);
  const pointChangeRef = useRef<Promise<void>>();
  const scrollRef = useRef<Promise<void>>();
  const pendingScrollRef = useRef<PageScroll>();
  const pagesRef = useRef<CapturedPage[]>([]);
  const browsePageRef = useRef<CapturedPage>();
  const browsingRef = useRef(false);
  const freezeRef = useRef<Promise<void>>();
  const pendingStrokeRef = useRef<{
    points: Point[];
    ended: boolean;
    number: number;
  }>();
  const strokeReadyRef = useRef<Promise<void>>();
  const nativeInputRef = useRef<(input: BrowseInput) => void>();
  const finalizingRef = useRef(false);
  const [points, setPoints] = useState(pointsRef.current);
  const [imageSize, setImageSize] = useState({ width: 1, height: 1 });
  const [busy, setBusy] = useState(false);
  const [finishing, setFinishing] = useState(false);
  const [actionFailed, setActionFailed] = useState(false);
  const [scrolling, setScrolling] = useState(false);
  const [browsing, setBrowsing] = useState(false);
  const [freezing, setFreezing] = useState(false);
  const [scrollFailed, setScrollFailed] = useState(false);
  const [color, setColor] = useState("#ff2d55");
  const [brushSize, setBrushSize] = useState(7);
  const [streamText, setStreamText] = useState<StreamTextEvent>({
    committed: "",
    tentative: "",
  });

  const updatePoints = useCallback((updated: AnnotationPoint[]) => {
    pointsRef.current = updated;
    setPoints(updated);
  }, []);

  const syncPointCount = useCallback(
    (count: number) => {
      const currentCount = pointCountRef.current;
      if (count <= currentCount) return;
      pointCountRef.current = count;
      drawingRef.current = false;
      lastPointRef.current = undefined;
      if (pendingStrokeRef.current) pendingStrokeRef.current.ended = true;
      updatePoints([
        ...pointsRef.current,
        ...Array.from({ length: count - currentCount }, (_, index) => ({
          number: currentCount + index + 1,
        })),
      ]);
    },
    [updatePoints],
  );

  const drawSnapshot = useCallback(() => {
    const canvas = canvasRef.current;
    const image = snapshotImageRef.current;
    if (!canvas || !image) return;
    const context = canvas.getContext("2d");
    if (!context) return;
    context.clearRect(0, 0, canvas.width, canvas.height);
    context.drawImage(image, 0, 0, canvas.width, canvas.height);
  }, []);

  const restoreBrowsePage = useCallback(() => {
    const canvas = canvasRef.current;
    const previous = browsePageRef.current;
    if (previous) {
      const context = canvas?.getContext("2d");
      if (!context) throw new Error("Cannot restore the annotated screen");
      context.drawImage(previous.canvas, 0, 0);
      const last = previous.points[previous.points.length - 1]?.number ?? 0;
      updatePoints([
        ...previous.points,
        ...pointsRef.current.filter((point) => point.number > last),
      ]);
    }
    browsePageRef.current = undefined;
    browsingRef.current = false;
    pendingStrokeRef.current = undefined;
    setBrowsing(false);
  }, [updatePoints]);

  const freezePage = useCallback((): Promise<void> => {
    if (freezeRef.current) return freezeRef.current;
    if (!browsingRef.current) return Promise.resolve();
    const sessionId = sessionIdRef.current;
    pendingScrollRef.current = undefined;
    setFreezing(true);
    setScrollFailed(false);
    const operation = (async () => {
      try {
        await scrollRef.current;
        if (sessionIdRef.current !== sessionId || !browsingRef.current) return;
        const latest = await invoke<ScreenAnnotationSnapshot>(
          "freeze_screen_annotation",
          { sessionId, captureId: captureIdRef.current },
        );
        if (sessionIdRef.current !== sessionId) return;
        captureIdRef.current = latest.capture_id;
        syncPointCount(latest.point_count);
        const image = new Image();
        image.crossOrigin = "anonymous";
        image.src = convertFileSrc(latest.path);
        await image.decode();
        if (sessionIdRef.current !== sessionId) return;
        const canvas = canvasRef.current;
        const previous = browsePageRef.current;
        const context = canvas?.getContext("2d");
        if (!canvas || !context || !previous)
          throw new Error("Cannot restore the browsed annotation");
        if (latest.capture_id !== displayedCaptureIdRef.current) {
          pagesRef.current.push(previous);
          context.drawImage(image, 0, 0);
          const newPoints = pointsRef.current.filter(
            (point) =>
              !previous.points.some((old) => old.number === point.number),
          );
          updatePoints(
            newPoints.length ? newPoints : [{ number: pointCountRef.current }],
          );
        } else {
          context.drawImage(previous.canvas, 0, 0);
        }
        snapshotImageRef.current = image;
        displayedCaptureIdRef.current = latest.capture_id;
        browsePageRef.current = undefined;
        browsingRef.current = false;
        setBrowsing(false);
        if (!finalizingRef.current) {
          await invoke("show_screen_annotation", {
            sessionId,
            captureId: latest.capture_id,
          });
        }
        void emit("screen-annotation-preview-drawn", {
          session_id: sessionId,
          capture_id: latest.capture_id,
        }).catch(console.error);
      } catch (error) {
        if (sessionIdRef.current === sessionId) {
          restoreBrowsePage();
          setScrollFailed(true);
          if (!finalizingRef.current)
            await invoke("show_screen_annotation", {
              sessionId,
              captureId: captureIdRef.current,
            });
        }
        throw error;
      } finally {
        if (sessionIdRef.current === sessionId) {
          freezeRef.current = undefined;
          setFreezing(false);
        }
      }
    })();
    freezeRef.current = operation;
    return operation;
  }, [restoreBrowsePage, syncPointCount, updatePoints]);

  const finalize = useCallback(async () => {
    const sessionId = sessionIdRef.current;
    if (sessionId === undefined) return;
    finalizingRef.current = true;
    if (pendingStrokeRef.current) pendingStrokeRef.current.ended = true;
    pendingScrollRef.current = undefined;
    setFinishing(true);
    await snapshotReadyRef.current;
    await pointChangeRef.current;
    await scrollRef.current;
    await freezePage();
    await strokeReadyRef.current;
    if (sessionIdRef.current !== sessionId) return;
    const canvas = canvasRef.current;
    if (!canvas) throw new Error("The annotation canvas is unavailable");
    const pages = [
      ...pagesRef.current,
      preservePage(canvas, pointsRef.current),
    ];
    const pngs = pages.map((page, index) =>
      renderPage(
        page,
        pages.length > 1
          ? t("screenAnnotation.screenNumber", { number: index + 1 })
          : undefined,
      ),
    );
    await invoke("submit_screen_annotation", {
      sessionId,
      pngDataUrl: pngs[0],
      additionalPngDataUrls: pngs.slice(1),
      pointCount: pointCountRef.current,
    });
  }, [t, freezePage]);

  const scrollPage = useCallback(
    (input: PageScroll) => {
      const sessionId = sessionIdRef.current;
      const canvas = canvasRef.current;
      if (
        sessionId === undefined ||
        !canvas ||
        !snapshotImageRef.current ||
        finalizingRef.current ||
        freezeRef.current ||
        pointChangeRef.current
      )
        return;
      const pending = pendingScrollRef.current;
      pendingScrollRef.current = {
        ...input,
        deltaX: (pending?.deltaX ?? 0) + input.deltaX,
        deltaY: (pending?.deltaY ?? 0) + input.deltaY,
      };
      if (scrollRef.current) return;
      drawingRef.current = false;
      lastPointRef.current = undefined;
      setScrolling(true);
      setScrollFailed(false);
      const operation = (async () => {
        try {
          await snapshotReadyRef.current;
          if (sessionIdRef.current !== sessionId) return;
          if (!browsingRef.current) {
            const context = canvas.getContext("2d");
            if (!context) throw new Error("Cannot release the frozen screen");
            browsePageRef.current = preservePage(canvas, pointsRef.current);
            browsingRef.current = true;
            setBrowsing(true);
            context.clearRect(0, 0, canvas.width, canvas.height);
          }
          while (pendingScrollRef.current) {
            const queued = pendingScrollRef.current;
            const dx = Math.max(-32767, Math.min(32767, queued.deltaX));
            const dy = Math.max(-32767, Math.min(32767, queued.deltaY));
            pendingScrollRef.current =
              queued.deltaX !== dx || queued.deltaY !== dy
                ? {
                    ...queued,
                    deltaX: queued.deltaX - dx,
                    deltaY: queued.deltaY - dy,
                  }
                : undefined;
            const latest = await invoke<ScreenAnnotationSnapshot>(
              "scroll_screen_annotation",
              {
                sessionId,
                captureId: captureIdRef.current,
                deltaX: dx,
                deltaY: dy,
                x: queued.x,
                y: queued.y,
              },
            );
            if (sessionIdRef.current !== sessionId) return;
            captureIdRef.current = latest.capture_id;
            syncPointCount(latest.point_count);
          }
        } catch (error) {
          console.error("Failed to refresh the annotated page:", error);
          if (sessionIdRef.current === sessionId) {
            pendingScrollRef.current = undefined;
            restoreBrowsePage();
            setScrollFailed(true);
            if (!finalizingRef.current)
              await invoke("show_screen_annotation", {
                sessionId,
                captureId: captureIdRef.current,
              });
          }
        } finally {
          if (sessionIdRef.current === sessionId) {
            scrollRef.current = undefined;
            setScrolling(false);
          }
        }
      })();
      scrollRef.current = operation;
    },
    [syncPointCount, restoreBrowsePage],
  );

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const wheel = (event: WheelEvent) => {
      event.preventDefault();
      const rect = canvas.getBoundingClientRect();
      const unit =
        event.deltaMode === 1 ? 40 : event.deltaMode === 2 ? rect.height : 1;
      const deltaX = Math.round(event.deltaX * unit);
      const deltaY = -Math.round(event.deltaY * unit);
      if (deltaX === 0 && deltaY === 0) return;
      scrollPage({
        x: Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width)),
        y: Math.max(0, Math.min(1, (event.clientY - rect.top) / rect.height)),
        deltaX,
        deltaY,
      });
    };
    canvas.addEventListener("wheel", wheel, { passive: false });
    return () => canvas.removeEventListener("wheel", wheel);
  }, [scrollPage]);

  const advancePoint = useCallback(() => {
    const sessionId = sessionIdRef.current;
    if (
      sessionId === undefined ||
      pointChangeRef.current ||
      scrollRef.current ||
      finalizingRef.current
    )
      return;
    setBusy(true);
    setActionFailed(false);
    drawingRef.current = false;
    lastPointRef.current = undefined;
    const operation = (async () => {
      try {
        const number = await invoke<number>("advance_screen_annotation", {
          sessionId,
          pointNumber: pointCountRef.current,
        });
        if (sessionIdRef.current === sessionId) {
          syncPointCount(number);
        }
      } catch (error) {
        console.error("Failed to advance the annotation point:", error);
        if (sessionIdRef.current === sessionId && !finalizingRef.current)
          setActionFailed(true);
      } finally {
        if (sessionIdRef.current === sessionId) {
          pointChangeRef.current = undefined;
          setBusy(false);
        }
      }
    })();
    pointChangeRef.current = operation;
  }, [syncPointCount]);

  const complete = async () => {
    setActionFailed(false);
    try {
      await invoke("complete_screen_annotation");
    } catch (error) {
      console.error("Failed to complete the annotation:", error);
      setActionFailed(true);
    }
  };

  useEffect(() => {
    let disposed = false;
    let unlistenFinish: (() => void) | undefined;
    let unlistenReset: (() => void) | undefined;
    let unlistenStream: (() => void) | undefined;
    let unlistenNext: (() => void) | undefined;
    let unlistenBrowse: (() => void) | undefined;

    const loadSnapshot = (snapshot: ScreenAnnotationSnapshot) => {
      if (disposed) return;
      sessionIdRef.current = snapshot.session_id;
      captureIdRef.current = snapshot.capture_id;
      displayedCaptureIdRef.current = snapshot.capture_id;
      pointCountRef.current = snapshot.point_count;
      finalizingRef.current = false;
      pointChangeRef.current = undefined;
      scrollRef.current = undefined;
      pendingScrollRef.current = undefined;
      browsePageRef.current = undefined;
      browsingRef.current = false;
      freezeRef.current = undefined;
      pendingStrokeRef.current = undefined;
      strokeReadyRef.current = undefined;
      pagesRef.current = [];
      snapshotImageRef.current = undefined;
      drawingRef.current = false;
      lastPointRef.current = undefined;
      updatePoints(
        Array.from({ length: snapshot.point_count }, (_, index) => ({
          number: index + 1,
        })),
      );
      setFinishing(false);
      setBusy(false);
      setActionFailed(false);
      setScrolling(false);
      setBrowsing(false);
      setFreezing(false);
      setScrollFailed(false);

      const image = new Image();
      image.crossOrigin = "anonymous";
      const loaded = new Promise<void>((resolve, reject) => {
        image.onload = () => resolve();
        image.onerror = () =>
          reject(new Error("Cannot load the captured screenshot"));
      });
      snapshotReadyRef.current = loaded.then(async () => {
        if (disposed || sessionIdRef.current !== snapshot.session_id) return;
        const canvas = canvasRef.current;
        if (!canvas) throw new Error("The annotation canvas is unavailable");
        canvas.width = snapshot.width;
        canvas.height = snapshot.height;
        setImageSize({ width: snapshot.width, height: snapshot.height });
        snapshotImageRef.current = image;
        drawSnapshot();
        const latest = await invoke<ScreenAnnotationSnapshot>(
          "get_screen_annotation_snapshot",
        );
        if (disposed || sessionIdRef.current !== latest.session_id) return;
        syncPointCount(latest.point_count);
      });
      void snapshotReadyRef.current
        .then(async () => {
          if (
            disposed ||
            sessionIdRef.current !== snapshot.session_id ||
            finalizingRef.current
          )
            return;
          await invoke("show_screen_annotation", {
            sessionId: snapshot.session_id,
            captureId: snapshot.capture_id,
          });
        })
        .catch((error) => {
          console.error("Failed to prepare the annotation screenshot:", error);
          if (!disposed && sessionIdRef.current === snapshot.session_id) {
            setActionFailed(true);
          }
        });
      image.src = convertFileSrc(snapshot.path);
    };

    const initialize = async () => {
      [
        unlistenFinish,
        unlistenReset,
        unlistenStream,
        unlistenNext,
        unlistenBrowse,
      ] = await Promise.all([
        listen("finish-screen-annotation", () => {
          const sessionId = sessionIdRef.current;
          void finalize().catch((error) => {
            console.error("Failed to submit the numbered screenshot:", error);
            if (sessionIdRef.current === sessionId) setActionFailed(true);
          });
        }),
        listen<ScreenAnnotationSnapshot>("reset-screen-annotation", (event) => {
          setStreamText({ committed: "", tentative: "" });
          loadSnapshot(event.payload);
        }),
        events.streamTextEvent.listen((event) => setStreamText(event.payload)),
        listen<ScreenAnnotationSnapshot>(
          "annotation-point-changed",
          (event) => {
            if (sessionIdRef.current === event.payload.session_id) {
              syncPointCount(event.payload.point_count);
            }
          },
        ),
        listen<BrowseInput>("annotation-browse-input", (event) => {
          nativeInputRef.current?.(event.payload);
        }),
      ]);
      if (disposed) {
        unlistenFinish();
        unlistenReset();
        unlistenStream();
        unlistenNext();
        unlistenBrowse();
        return;
      }
      const snapshot = await invoke<ScreenAnnotationSnapshot>(
        "get_screen_annotation_snapshot",
      ).catch(() => undefined);
      if (snapshot) loadSnapshot(snapshot);
    };

    void initialize();
    return () => {
      disposed = true;
      unlistenFinish?.();
      unlistenReset?.();
      unlistenStream?.();
      unlistenNext?.();
      unlistenBrowse?.();
    };
  }, [drawSnapshot, finalize, updatePoints, syncPointCount]);

  const pointFromEvent = (event: React.PointerEvent<HTMLCanvasElement>) => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    const rect = canvas.getBoundingClientRect();
    return {
      x: (event.clientX - rect.left) * (canvas.width / rect.width),
      y: (event.clientY - rect.top) * (canvas.height / rect.height),
    };
  };

  const beginStroke = (event: React.PointerEvent<HTMLCanvasElement>) => {
    event.preventDefault();
    if (pointChangeRef.current || freezeRef.current || finalizingRef.current)
      return;
    const point = pointFromEvent(event);
    if (!point) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    if (browsingRef.current) {
      beginBufferedStroke(point);
      return;
    }
    startStroke(point);
  };

  const beginBufferedStroke = (point: Point) => {
    const sessionId = sessionIdRef.current;
    const gesture = {
      points: [point],
      ended: false,
      number: pointCountRef.current,
    };
    pendingStrokeRef.current = gesture;
    const operation = freezePage()
      .then(() => {
        if (
          pendingStrokeRef.current !== gesture ||
          sessionIdRef.current !== sessionId
        )
          return;
        pendingStrokeRef.current = undefined;
        startStroke(gesture.points[0], gesture.number);
        for (const next of gesture.points.slice(1)) paintStroke(next);
        if (gesture.ended) {
          drawingRef.current = false;
          lastPointRef.current = undefined;
        }
      })
      .catch((error) => {
        console.error("Failed to freeze the page for drawing:", error);
      });
    strokeReadyRef.current = operation;
  };

  const startStroke = (point: Point, number = pointCountRef.current) => {
    drawingRef.current = true;
    lastPointRef.current = point;
    const current = pointsRef.current.find((entry) => entry.number === number);
    if (!current?.position) {
      updatePoints(
        [
          ...pointsRef.current.filter((entry) => entry.number !== number),
          { number, position: point },
        ].sort((a, b) => a.number - b.number),
      );
    }
  };

  const continueStroke = (event: React.PointerEvent<HTMLCanvasElement>) => {
    if (finalizingRef.current) return;
    const point = pointFromEvent(event);
    if (!point) return;
    if (pendingStrokeRef.current) {
      if (!pendingStrokeRef.current.ended)
        pendingStrokeRef.current.points.push(point);
      return;
    }
    paintStroke(point);
  };

  const paintStroke = (point: Point) => {
    if (!drawingRef.current) return;
    const canvas = canvasRef.current;
    const previous = lastPointRef.current;
    if (!canvas || !point || !previous) return;

    const context = canvas.getContext("2d");
    if (!context) return;
    const scale = canvas.width / canvas.getBoundingClientRect().width;
    context.strokeStyle = color;
    context.lineWidth = brushSize * scale;
    context.lineCap = "round";
    context.lineJoin = "round";
    context.beginPath();
    context.moveTo(previous.x, previous.y);
    context.lineTo(point.x, point.y);
    context.stroke();
    lastPointRef.current = point;
  };

  const endStroke = () => {
    if (pendingStrokeRef.current) pendingStrokeRef.current.ended = true;
    drawingRef.current = false;
    lastPointRef.current = undefined;
  };

  nativeInputRef.current = (input) => {
    if (input.session_id !== sessionIdRef.current || finalizingRef.current)
      return;
    if (input.kind === "wheel") {
      scrollPage({
        x: input.x,
        y: input.y,
        deltaX: input.delta_x,
        deltaY: input.delta_y,
      });
      return;
    }
    const canvas = canvasRef.current;
    if (!canvas) return;
    const point = { x: input.x * canvas.width, y: input.y * canvas.height };
    if (input.kind === "down") {
      if (browsingRef.current && !freezeRef.current) beginBufferedStroke(point);
    } else if (input.kind === "move") {
      if (pendingStrokeRef.current) {
        if (!pendingStrokeRef.current.ended)
          pendingStrokeRef.current.points.push(point);
      } else {
        paintStroke(point);
      }
    } else {
      endStroke();
    }
  };

  return (
    <main
      className={`annotation-stage${browsing ? " annotation-browsing" : ""}`}
      aria-busy={scrolling || freezing}
    >
      <canvas
        ref={canvasRef}
        className={`annotation-canvas${scrolling ? " annotation-scrolling" : ""}`}
        aria-label={t("screenAnnotation.scrollHint")}
        onPointerDown={beginStroke}
        onPointerMove={continueStroke}
        onPointerUp={endStroke}
        onPointerCancel={endStroke}
      />
      <div className="annotation-markers" aria-live="polite">
        {points.map((point) => {
          const scale =
            imageSize.width /
            (canvasRef.current?.getBoundingClientRect().width ||
              imageSize.width);
          const position = markerPosition(
            point,
            imageSize.width,
            imageSize.height,
            scale,
          );
          return (
            <span
              key={point.number}
              className="annotation-marker"
              aria-label={t("screenAnnotation.point", { number: point.number })}
              style={{
                left: `${(position.x / imageSize.width) * 100}%`,
                top: `${(position.y / imageSize.height) * 100}%`,
              }}
            >
              {point.number}
            </span>
          );
        })}
      </div>
      <div className="annotation-hud">
        <div className="annotation-toolbar">
          <span className="annotation-recording-dot" />
          <input
            className="annotation-color"
            type="color"
            aria-label={t("screenAnnotation.inkColor")}
            value={color}
            onChange={(event) => setColor(event.target.value)}
          />
          <input
            className="annotation-size"
            type="range"
            aria-label={t("screenAnnotation.strokeWidth")}
            min="2"
            max="24"
            value={brushSize}
            onChange={(event) => setBrushSize(Number(event.target.value))}
          />
          <button
            className="annotation-button"
            title={t("screenAnnotation.clearInk")}
            aria-label={t("screenAnnotation.clearInk")}
            onClick={drawSnapshot}
            disabled={busy || scrolling || finishing}
          >
            <Trash2 aria-hidden="true" />
          </button>
          <button
            className="annotation-button annotation-next"
            title={t("screenAnnotation.nextPointShortcut")}
            onClick={advancePoint}
            disabled={
              busy ||
              scrolling ||
              finishing ||
              !points[points.length - 1]?.position
            }
          >
            {t("screenAnnotation.nextPoint")}
            <ArrowRight aria-hidden="true" />
          </button>
          <button
            className="annotation-button annotation-done"
            title={t("settings.history.save")}
            aria-label={t("settings.history.save")}
            onClick={() => void complete()}
            disabled={busy || scrolling || finishing}
          >
            <Check aria-hidden="true" />
          </button>
        </div>
        {actionFailed && (
          <div className="annotation-transcript" role="alert">
            {t("screenAnnotation.actionFailed")}
          </div>
        )}
        {scrollFailed && (
          <div className="annotation-transcript" role="alert">
            {t("screenAnnotation.scrollFailed")}
          </div>
        )}
        {(streamText.committed || streamText.tentative) && (
          <div className="annotation-transcript" aria-live="polite">
            <span>{streamText.committed}</span>
            {streamText.committed && streamText.tentative ? " " : ""}
            <span className="annotation-tentative">{streamText.tentative}</span>
          </div>
        )}
      </div>
    </main>
  );
};

export default ScreenAnnotation;
