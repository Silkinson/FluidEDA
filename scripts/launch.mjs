// Shared Chromium launch for the browser harnesses (Phase 5c, wgpu 30).
//
// GPU=webgpu → real WebGPU on the host GPU (headless Chromium needs
//   --enable-unsafe-webgpu; --use-angle=metal puts it on Metal on macOS).
// GPU=webgpu-sw → WebGPU on SwiftShader (CPU).
// GPU=webgl-metal → WebGL2 on ANGLE-Metal (the host GPU) — browser_bench's
//   historical mode (all pre-wgpu-30 fps numbers).
// default → Chromium's headless default: no WebGPU adapter, so the app falls
//   back to WebGL2 on SwiftShader — what every pre-wgpu-30 number measured.
// The app reports the backend it got in __fluideda_metrics.backend.
// UNCAP=1 lifts the display-rate limit (no vsync), so frame rates above the
// display's 120 Hz can be told apart (Phase 7.6).
import { chromium } from 'playwright';

export const GPU_MODE = process.env.GPU || 'webgl';

export function launchArgs(mode = GPU_MODE) {
  switch (mode) {
    case 'webgpu':
      return ['--enable-unsafe-webgpu', '--use-angle=metal'];
    case 'webgpu-sw':
      return ['--enable-unsafe-webgpu'];
    case 'webgl-metal':
      return ['--use-angle=metal', '--ignore-gpu-blocklist'];
    default:
      return [];
  }
}

const UNCAP = process.env.UNCAP ? ['--disable-frame-rate-limit', '--disable-gpu-vsync'] : [];

export function launch(opts = {}) {
  return chromium.launch({ ...opts, args: [...(opts.args || []), ...launchArgs(), ...UNCAP] });
}
