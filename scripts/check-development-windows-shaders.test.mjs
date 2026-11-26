import assert from "node:assert/strict";
import { verifyEmbeddedWindowsShaders } from "./check-development-windows-shaders.mjs";

const names = ["QUAD", "OPAQUE_QUAD", "SHADOW", "PATH_TILE", "UNDERLINE",
  "MONOCHROME_SPRITE", "SUBPIXEL_SPRITE", "POLYCHROME_SPRITE"]
  .flatMap((name) => [`${name}_VERTEX_BYTES`, `${name}_FRAGMENT_BYTES`]);
const buffers = names.map((_, index) => Buffer.concat([Buffer.from("DXBC"), Buffer.alloc(32, index)]));
const bindings = names.map((name, index) => `const ${name}: &[u8] = &[${[...buffers[index]].join(",")}];`).join("\n");
const binary = Buffer.concat(buffers);
assert.deepEqual(verifyEmbeddedWindowsShaders(binary, bindings), { status: "passed", shaderCount: 16, format: "DXBC" });
for (const encoding of ["utf8", "utf16le"]) {
  for (const path of ["src/shaders.hlsl", "src\\shaders.hlsl", "rounded_clip_shaders.hlsl"]) {
    assert.throws(() => verifyEmbeddedWindowsShaders(Buffer.concat([binary, Buffer.from(path, encoding)]), bindings), /Nonportable GPUI runtime shader path/);
  }
}
assert.throws(() => verifyEmbeddedWindowsShaders(binary, ""), /Missing generated shader/);
assert.throws(() => verifyEmbeddedWindowsShaders(Buffer.concat(buffers.slice(1)), bindings), /not embedded/);
assert.throws(() => verifyEmbeddedWindowsShaders(binary, bindings + "\n" + bindings.split("\n")[0]), /Unexpected shader binding/);
assert.throws(() => verifyEmbeddedWindowsShaders(binary, bindings.replace("68,88,66,67", "68,88,66,999")), /Invalid shader byte array/);
console.log("Portable GPUI shader gate: embedded bytes and regression cases passed");
