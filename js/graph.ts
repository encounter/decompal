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
        x: (unit.x / 100) * width,
        y: (unit.y / 100) * height,
        w: (unit.w / 100) * width,
        h: (unit.h / 100) * height,
    };
}

const TOOLTIP_PADDING = 10;
const TOOLTIP_MARGIN = 10;

const drawTooltip = (ctx: CanvasRenderingContext2D, unit: Unit, width: number, height: number) => {
    const {x, y, w, h} = unitBounds(unit, width, height);
    const text = `${unit.name} • ${unit.fuzzy_match_percent.toFixed(2)}%`;
    const m = ctx.measureText(text);
    const hw = m.actualBoundingBoxRight + m.actualBoundingBoxLeft + TOOLTIP_PADDING * 2;
    const bh = m.actualBoundingBoxAscent + m.actualBoundingBoxDescent + TOOLTIP_PADDING * 2;
    let bx = x + (w - hw) / 2 + TOOLTIP_PADDING;
    let by = y - TOOLTIP_MARGIN;
    if (bx < 0) {
        bx = 0;
    }
    if (bx + hw > width) {
        bx = width - hw;
    }
    if (by - bh < 0) {
        by = y + h + bh + TOOLTIP_MARGIN;
    }
    ctx.fillStyle = "rgba(255, 255, 255, 0.8)";
    ctx.fillRect(bx, by - m.actualBoundingBoxAscent - TOOLTIP_PADDING * 2, hw, bh);
    ctx.fillStyle = "#000";
    ctx.fillText(text, bx + TOOLTIP_PADDING, by - TOOLTIP_PADDING);
};

let hovered = null;
let dirty = false;

const draw = (canvas: HTMLCanvasElement, units: Unit[]) => {
    if (!canvas.getContext) {
        return;
    }
    const ratio = window.devicePixelRatio;
    const {width, height} = canvas.getBoundingClientRect();
    const renderWidth = width * ratio;
    const renderHeight = height * ratio;
    if (!dirty && canvas.width === renderWidth && canvas.height === renderHeight) {
        // Nothing changed
        return;
    }
    dirty = false;
    if (canvas.width !== renderWidth || canvas.height !== renderHeight) {
        canvas.width = renderWidth;
        canvas.height = renderHeight;
    }
    const style = getComputedStyle(canvas);
    const fontWeight = style.getPropertyValue('--font-weight') || 'normal';
    const fontSize = style.getPropertyValue('--font-size') || '16px';
    const fontFamily = style.getPropertyValue('--font-family') || 'sans-serif';
    const ctx = canvas.getContext("2d");
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    ctx.clearRect(0, 0, width, height);
    ctx.font = `${fontWeight} ${fontSize} ${fontFamily}`;
    ctx.lineWidth = 1;
    ctx.strokeStyle = "#000";
    for (const unit of units) {
        const {x, y, w, h} = unitBounds(unit, width, height);
        ctx.fillStyle = unit.color;
        ctx.fillRect(x, y, w, h);
        ctx.strokeRect(x, y, w, h);
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
    canvas.addEventListener("mousemove", handleHover);
    canvas.addEventListener("mouseleave", handleLeave);
    canvas.addEventListener("touchmove", (e) => handleHover(e.touches[0]));
    canvas.addEventListener("touchend", handleLeave);
    draw(canvas, units);
};

// noinspection JSUnusedGlobalSymbols
interface Window {
    drawGraph: (id: string, units: Unit[]) => void;
}

window.drawGraph = drawGraph;
