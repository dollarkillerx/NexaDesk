// A pasted picture, made as small to send as it can be without spoiling it. A PNG stays
// as it is up to a size, which screenshots of text mostly stay under. A larger one with
// no transparency, most often a photo, goes as a JPEG of high quality, a fraction of the
// size; a JPEG goes as it is. Anything else becomes one of those. The host turns what is
// not a PNG into one for its clipboard.

/** A PNG up to this size goes as it is. */
export const KEEP_PNG_UNDER = 2 * 1024 * 1024;
export const JPEG_QUALITY = 0.9;
/** A JPEG has to save at least this much of the PNG to be worth what it loses. */
const JPEG_AT_MOST = 0.7;

function canvasOf(w, h) {
  if (typeof OffscreenCanvas === "function") return new OffscreenCanvas(w, h);
  return Object.assign(document.createElement("canvas"), { width: w, height: h });
}

function blobOf(canvas, type, quality) {
  if (canvas.convertToBlob) return canvas.convertToBlob({ type, quality });
  return new Promise((done) => canvas.toBlob(done, type, quality));
}

/** Whether every pixel is fully opaque, read a few rows at a time. */
function opaque(ctx, w, h) {
  const rows = Math.max(1, Math.floor((4 * 1024 * 1024) / (w * 4)));
  for (let y = 0; y < h; y += rows) {
    const data = ctx.getImageData(0, y, w, Math.min(rows, h - y)).data;
    for (let i = 3; i < data.length; i += 4) if (data[i] !== 255) return false;
  }
  return true;
}

/** What to send for a pasted picture: `{ blob, kind }`, kind "png" or "jpeg". */
export async function forSending(file) {
  if (file.type === "image/jpeg") return { blob: file, kind: "jpeg" };
  if (file.type === "image/png" && file.size <= KEEP_PNG_UNDER) return { blob: file, kind: "png" };
  const bitmap = await createImageBitmap(file);
  const { width: w, height: h } = bitmap;
  const canvas = canvasOf(w, h);
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  ctx.drawImage(bitmap, 0, 0);
  bitmap.close?.();
  const png = file.type === "image/png" ? file : await blobOf(canvas, "image/png");
  if (png.size <= KEEP_PNG_UNDER || !opaque(ctx, w, h)) return { blob: png, kind: "png" };
  const jpeg = await blobOf(canvas, "image/jpeg", JPEG_QUALITY);
  return jpeg.size <= png.size * JPEG_AT_MOST ? { blob: jpeg, kind: "jpeg" } : { blob: png, kind: "png" };
}
