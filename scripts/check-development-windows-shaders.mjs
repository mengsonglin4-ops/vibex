import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const MODULES = [
  "QUAD", "OPAQUE_QUAD", "SHADOW", "PATH_TILE", "UNDERLINE",
  "MONOCHROME_SPRITE", "SUBPIXEL_SPRITE", "POLYCHROME_SPRITE"
];
const EXPECTED = MODULES.flatMap((name) => [`${name}_VERTEX_BYTES`, `${name}_FRAGMENT_BYTES`]);

// A --probe invocation returns before DirectX initialization. Check the actual
// executable against the GPUI build script's FXC output so a cloud-only HLSL
// path cannot pass packaging even on a runner with no usable graphics adapter.
export function verifyEmbeddedWindowsShaders(binary, bindings) {
  for (const path of ["src/shaders.hlsl", "src\\shaders.hlsl", "rounded_clip_shaders.hlsl"]) {
    if (["utf8", "utf16le"].some((encoding) => binary.includes(Buffer.from(path, encoding)))) {
      throw new Error(`Nonportable GPUI runtime shader path remains in executable: ${path}`);
    }
  }
  const shaders = new Map();
  const pattern = /const\s+(\w+)\s*:\s*&\[u8\]\s*=\s*&\[([\s\S]*?)\];/g;
  for (const [, name, values] of bindings.matchAll(pattern)) {
    if (!EXPECTED.includes(name) || shaders.has(name)) throw new Error(`Unexpected shader binding: ${name}`);
    const numbers = values.split(",").map((value) => value.trim()).filter(Boolean).map(Number);
    if (numbers.some((value) => !Number.isInteger(value) || value < 0 || value > 255)) {
      throw new Error(`Invalid shader byte array: ${name}`);
    }
    const bytes = Buffer.from(numbers);
    if (bytes.length < 32 || bytes.toString("ascii", 0, 4) !== "DXBC") {
      throw new Error(`Expected FXC DXBC bytecode: ${name}`);
    }
    shaders.set(name, bytes);
  }
  for (const name of EXPECTED) {
    const bytes = shaders.get(name);
    if (!bytes) throw new Error(`Missing generated shader: ${name}`);
    if (!binary.includes(bytes)) throw new Error(`Shader bytecode is not embedded in executable: ${name}`);
  }
  return { status: "passed", shaderCount: shaders.size, format: "DXBC" };
}

export function checkDevelopmentWindowsShaders(binaryDir) {
  const binary = readFileSync(join(binaryDir, "vibex-desktop.exe"));
  const buildDir = join(binaryDir, "build");
  const candidates = existsSync(buildDir) ? readdirSync(buildDir)
    .filter((name) => name.startsWith("gpui-fast-windows-"))
    .map((name) => join(buildDir, name, "out/shaders_bytes.rs"))
    .filter(existsSync) : [];
  // Validate the known bad runtime path even when a debug build produced no
  // shaders_bytes.rs at all, giving the direct fix for the portability failure.
  if (candidates.length !== 1) {
    verifyEmbeddedWindowsShaders(binary, "");
    throw new Error(`Expected one fresh GPUI shader binding file, found ${candidates.length}`);
  }
  return verifyEmbeddedWindowsShaders(binary, readFileSync(candidates[0], "utf8"));
}
