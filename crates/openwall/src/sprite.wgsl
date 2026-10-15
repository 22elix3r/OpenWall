// SPDX-License-Identifier: AGPL-3.0-or-later
// Original OpenWall shader. No Wallpaper Engine shader code is used.
struct Output {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
    @location(1) opacity: f32,
}
@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;

@vertex fn vertex_main(@location(0) position: vec2f, @location(1) uv: vec2f, @location(2) opacity: f32) -> Output {
    return Output(vec4f(position, 0.0, 1.0), uv, opacity);
}
@fragment fn fragment_main(input: Output) -> @location(0) vec4f {
    let color = textureSample(image, image_sampler, input.uv);
    return vec4f(color.rgb, color.a * input.opacity);
}
