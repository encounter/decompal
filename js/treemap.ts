type Unit = {
    name: string;
    fuzzy_match_percent: number;
    color: string;
    x: number;
    y: number;
    w: number;
    h: number;
};

const unitBounds = (unit: Unit, width: number, height: number) => {
    return {
        x: unit.x * width,
        y: unit.y * height,
        w: unit.w * width,
        h: unit.h * height,
    };
}

const BORDER_RADIUS = 5;
const PADDING_W = 10;
const PADDING_H = 5;
const MARGIN = 5;

const drawTooltip = (ctx: CanvasRenderingContext2D, unit: Unit, width: number, height: number) => {
    const style = getComputedStyle(ctx.canvas);
    const fontWeight = style.getPropertyValue('--font-weight') || 'normal';
    const fontSize = style.getPropertyValue('--font-size') || '16px';
    const fontFamily = style.getPropertyValue('--font-family') || 'sans-serif';
    const tooltipBackground = style.getPropertyValue('--tooltip-background') || "#fff"
    const tooltipColor = style.getPropertyValue('--tooltip-color') || "#000"
    ctx.font = `${fontWeight} ${fontSize} ${fontFamily}`;
    ctx.textBaseline = "middle";

    const {x, y, w, h} = unitBounds(unit, width, height);
    const text = `${unit.name} • ${unit.fuzzy_match_percent.toFixed(2)}%`;
    const m = ctx.measureText(text);
    const bw = m.actualBoundingBoxRight + m.actualBoundingBoxLeft + PADDING_W * 2;
    const bh = m.fontBoundingBoxAscent + m.fontBoundingBoxDescent + PADDING_H * 2;
    const margin = isTouch ? MARGIN * 2 : MARGIN;
    let bx = x + (w - bw) / 2;
    let by = y - bh - margin;
    let ay = y;
    if (bx + bw > width) {
        bx = width - bw;
    }
    if (bx < 0) {
        bx = 0;
    }
    if (by < 0) {
        // Draw below the box
        by = y + h + margin;
        ay = y + h;
    }
    if (by + bh > height) {
        // Draw inside the box
        by = y + margin;
        ay = y;
    }
    ctx.fillStyle = tooltipBackground;
    ctx.beginPath();
    ctx.roundRect(bx, by, bw, bh, BORDER_RADIUS);
    // Arrow
    const ax = x + w / 2;
    if (ay < by) {
        // Top
        ctx.moveTo(ax, ay);
        ctx.lineTo(ax + margin, by);
        ctx.lineTo(ax - margin, by);
    } else {
        // Bottom
        ctx.moveTo(ax, ay);
        ctx.lineTo(ax + margin, by + bh);
        ctx.lineTo(ax - margin, by + bh);
    }
    ctx.fill();
    ctx.fillStyle = tooltipColor;
    ctx.fillText(text, bx + PADDING_W, by + bh / 2);
};

let hovered = null;
let dirty = false;
let isTouch = false;
let cachedCanvas: HTMLCanvasElement = null;

const setup = (ctx: CanvasRenderingContext2D, ratio: number, width: number, height: number) => {
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0); // Scale to device pixel ratio
    ctx.clearRect(0, 0, width, height);
    ctx.lineWidth = 1;
    ctx.strokeStyle = "#000";
}

const drawUnits = (ctx: CanvasRenderingContext2D, units: Unit[], width: number, height: number) => {
    for (const unit of units) {
        const {x, y, w, h} = unitBounds(unit, width, height);
        ctx.fillStyle = unit.color;
        ctx.beginPath();
        ctx.rect(x, y, w, h);
        ctx.fill();
        ctx.stroke();
    }
}

const draw = (canvas: HTMLCanvasElement, units: Unit[]) => {
    const {width, height} = canvas.getBoundingClientRect();
    const ratio = window.devicePixelRatio;
    const renderWidth = width * ratio;
    const renderHeight = height * ratio;
    if (!dirty && canvas.width === renderWidth && canvas.height === renderHeight) {
        // Nothing changed
        return;
    }
    dirty = false;
    // High DPI support
    if (canvas.width !== renderWidth || canvas.height !== renderHeight) {
        canvas.width = renderWidth;
        canvas.height = renderHeight;
    }
    // Update cached canvas if needed
    if (cachedCanvas.width !== renderWidth || cachedCanvas.height !== renderHeight) {
        cachedCanvas.width = renderWidth;
        cachedCanvas.height = renderHeight;
        const cachedCtx = cachedCanvas.getContext("2d");
        if (!cachedCtx) {
            return;
        }
        setup(cachedCtx, ratio, width, height);
        drawUnits(cachedCtx, units, width, height);
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) {
        return;
    }
    // Use 1:1 scale for rendering cached canvas
    setup(ctx, 1, renderWidth, renderHeight);
    ctx.drawImage(cachedCanvas, 0, 0);
    ctx.scale(ratio, ratio); // Restore device scale
    if (hovered) {
        const {x, y, w, h} = unitBounds(hovered, width, height);
        ctx.lineWidth = 2;
        ctx.strokeStyle = "#fff";
        ctx.strokeRect(x, y, w, h);
        drawTooltip(ctx, hovered, width, height);
    }
};

const findUnit = (canvas: HTMLCanvasElement, units: Unit[], clientX: number, clientY: number): Unit | null => {
    const {width, height, left, top} = canvas.getBoundingClientRect();
    const mx = clientX - left;
    const my = clientY - top;
    for (const unit of units) {
        const {x, y, w, h} = unitBounds(unit, width, height);
        if (mx >= x && mx <= x + w && my >= y && my <= y + h) {
            return unit;
        }
    }
    return null;
}

const drawTreemap = (id: string, clickable: boolean, units: Unit[]) => {
    const canvas = document.getElementById(id) as HTMLCanvasElement;
    if (!canvas || !canvas.getContext) {
        return;
    }
    if (!cachedCanvas) {
        cachedCanvas = document.createElement("canvas");
    }
    const queueDraw = () => requestAnimationFrame(() => draw(canvas, units));
    const resizeObserver = new ResizeObserver(queueDraw);
    resizeObserver.observe(canvas);
    const handleHover = ({clientX, clientY}: { clientX: number, clientY: number }) => {
        const unit = findUnit(canvas, units, clientX, clientY);
        if (unit === hovered) {
            return;
        }
        if (clickable) {
            canvas.style.cursor = unit ? "pointer" : "default";
        }
        hovered = unit;
        dirty = true;
        queueDraw();
    }
    const handleLeave = () => {
        if (!hovered) {
            return;
        }
        if (clickable) {
            canvas.style.cursor = "default";
        }
        hovered = null;
        dirty = true;
        queueDraw();
    };
    canvas.addEventListener("mousemove", (e) => {
        isTouch = false;
        handleHover(e);
    });
    canvas.addEventListener("mouseleave", handleLeave);
    canvas.addEventListener("touchmove", (e) => {
        isTouch = true;
        handleHover(e.touches[0]);
    });
    canvas.addEventListener("touchend", handleLeave);
    canvas.addEventListener("click", ({clientX, clientY}) => {
        const unit = findUnit(canvas, units, clientX, clientY);
        if (!unit || !unit.name || !clickable) {
            return;
        }
        const url = new URL(window.location.href);
        url.searchParams.set("unit", unit.name);
        window.location.href = url.toString();
    });
    draw(canvas, units);
};

// noinspection JSUnusedGlobalSymbols
interface Window {
    drawTreemap: (id: string, clickable: boolean, units: Unit[]) => void;
}

window.drawTreemap = drawTreemap;
