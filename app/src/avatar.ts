/** 居中裁成正方形时，从源图取出的矩形。 */
export interface CropRect {
  sx: number;
  sy: number;
  sw: number;
  sh: number;
}

/**
 * 任意宽高 → 居中正方形源裁剪矩形。
 * size 是导出边长，不参与源矩形计算（缩放在绘制时做）。
 */
export function coverCrop(width: number, height: number, _size = 256): CropRect {
  const side = Math.min(width, height);
  return {
    sx: Math.floor((width - side) / 2),
    sy: Math.floor((height - side) / 2),
    sw: side,
    sh: side,
  };
}

const AVATAR_SIZE = 256;
const AVATAR_QUALITY = 0.85;

/** 把选中的图片居中裁成 256×256，优先 WebP，不支持时 JPEG。 */
export async function cropAvatarFile(file: File): Promise<Blob> {
  const bmp = await createImageBitmap(file);
  try {
    const { sx, sy, sw, sh } = coverCrop(bmp.width, bmp.height, AVATAR_SIZE);
    const canvas = document.createElement("canvas");
    canvas.width = AVATAR_SIZE;
    canvas.height = AVATAR_SIZE;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("无法裁剪图片");
    ctx.drawImage(bmp, sx, sy, sw, sh, 0, 0, AVATAR_SIZE, AVATAR_SIZE);
    const webp = await canvasToBlob(canvas, "image/webp", AVATAR_QUALITY);
    if (webp && webp.type === "image/webp") return webp;
    const jpeg = await canvasToBlob(canvas, "image/jpeg", AVATAR_QUALITY);
    if (!jpeg) throw new Error("无法导出图片");
    return jpeg;
  } finally {
    bmp.close();
  }
}

function canvasToBlob(canvas: HTMLCanvasElement, type: string, quality: number): Promise<Blob | null> {
  return new Promise((resolve) => canvas.toBlob((b) => resolve(b), type, quality));
}
