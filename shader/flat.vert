#version 330

layout(location = 0) in vec2 pos;
layout(location = 1) in vec4 color;

uniform mat4 mvp;
uniform vec4 tint;

flat out vec4 v_color;

void main() {
    v_color = color * tint;
    gl_Position = mvp * vec4(pos, 0, 1);
}
