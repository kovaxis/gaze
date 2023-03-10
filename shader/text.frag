#version 330

in vec2 v_uv;
flat in vec4 tint;

uniform sampler2D glyph;

out vec4 color;

void main() {
    float bright = texture(glyph, v_uv).r;
    color = tint * vec4(bright);
}
