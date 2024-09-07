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
    ctx.font = `${fontWeight} ${fontSize} ${fontFamily}`;
    ctx.textBaseline = "middle";

    const {x, y, w, h} = unitBounds(unit, width, height);
    const text = `${unit.name} • ${unit.fuzzy_match_percent.toFixed(2)}%`;
    const m = ctx.measureText(text);
    const bw = m.actualBoundingBoxRight + m.actualBoundingBoxLeft + PADDING_W * 2;
    const bh = m.fontBoundingBoxAscent + m.fontBoundingBoxDescent + PADDING_H * 2;
    const margin = isTouch ? MARGIN * 2 : MARGIN;
    let bx = x + (w - bw) / 2 + PADDING_W;
    let by = y - bh - margin;
    let invY = false;
    if (bx + bw > width) {
        bx = width - bw;
    }
    if (bx < 0) {
        bx = 0;
    }
    if (by < 0) {
        by = y + h + margin;
        invY = true;
    }
    ctx.fillStyle = tooltipBackground;
    ctx.beginPath();
    ctx.roundRect(bx, by, bw, bh, BORDER_RADIUS);
    // Arrow
    const ax = x + w / 2;
    if (invY) {
        ctx.moveTo(ax, y + h);
        ctx.lineTo(ax + margin, y + h + margin);
        ctx.lineTo(ax - margin, y + h + margin);
    } else {
        ctx.moveTo(ax, y);
        ctx.lineTo(ax + margin, y - margin);
        ctx.lineTo(ax - margin, y - margin);
    }
    ctx.fill();
    ctx.fillStyle = "#000";
    ctx.fillText(text, bx + PADDING_W, by + bh / 2);
};

let hovered = null;
let dirty = false;
let isTouch = false;

const draw = (canvas: HTMLCanvasElement, units: Unit[]) => {
    if (!canvas.getContext) {
        return;
    }
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
    const ctx = canvas.getContext("2d");
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0); // Scale to device pixel ratio
    ctx.clearRect(0, 0, width, height);
    ctx.lineWidth = 1;
    ctx.strokeStyle = "#000";
    for (const unit of units) {
        const {x, y, w, h} = unitBounds(unit, width, height);
        ctx.fillStyle = unit.color;
        ctx.beginPath();
        ctx.rect(x, y, w, h);
        ctx.fill();
        ctx.stroke();
    }
    if (hovered) {
        const {x, y, w, h} = unitBounds(hovered, width, height);
        ctx.lineWidth = 2;
        ctx.strokeStyle = "#fff";
        ctx.strokeRect(x, y, w, h);
        drawTooltip(ctx, hovered, width, height);
    }
};

const drawGraph = (id: string, units: Unit[]) => {
    const canvas = document.getElementById(id) as HTMLCanvasElement;
    if (!canvas) {
        return;
    }
    const queueDraw = () => requestAnimationFrame(() => draw(canvas, units));
    const resizeObserver = new ResizeObserver(queueDraw);
    resizeObserver.observe(canvas);
    const handleHover = ({clientX, clientY}: { clientX: number, clientY: number }) => {
        const {width, height, left, top} = canvas.getBoundingClientRect();
        const mx = clientX - left;
        const my = clientY - top;
        const prev = hovered;
        hovered = null;
        for (const unit of units) {
            const {x, y, w, h} = unitBounds(unit, width, height);
            if (mx >= x && mx <= x + w && my >= y && my <= y + h) {
                hovered = unit;
                break;
            }
        }
        if (prev === hovered) {
            return;
        }
        dirty = true;
        queueDraw();
    }
    const handleLeave = () => {
        if (!hovered) {
            return;
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
    draw(canvas, units);
};

// noinspection JSUnusedGlobalSymbols
interface Window {
    drawGraph: (id: string, units: Unit[]) => void;
}

window.drawGraph = drawGraph;
