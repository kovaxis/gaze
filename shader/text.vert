#version 330

layout(location = 0) in vec2 pos;
layout(location = 1) in vec2 uv;
layout(location = 2) in vec4 color;

uniform mat4 mvp;

out vec2 v_uv;
flat out vec4 tint;

void main() {
    v_uv = uv;
    tint = color;
    gl_Position = mvp * vec4(pos, 0, 1);
}
