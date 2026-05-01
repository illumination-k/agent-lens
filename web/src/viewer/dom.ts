export function getElement<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (element === null) {
    throw new Error(`Missing #${id}`);
  }
  return element as T;
}

export function getCanvasContext(element: HTMLCanvasElement): CanvasRenderingContext2D {
  const canvasContext = element.getContext("2d");
  if (canvasContext === null) {
    throw new Error("Canvas 2D context is unavailable");
  }
  return canvasContext;
}

export function formatNumber(value: number | null): string {
  return value === null ? "n/a" : value.toFixed(1);
}

export function escapeHtml(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

export function escapeAttr(value: string): string {
  return escapeHtml(value).replaceAll('"', "&quot;");
}
