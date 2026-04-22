#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path
import textwrap

from PIL import Image, ImageDraw, ImageFont


ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "docs" / "assets"
SCALE = 2

BG = (8, 12, 13)
PANEL = (17, 26, 27)
PANEL_2 = (22, 34, 35)
LINE = (230, 225, 211, 28)
TEXT = (242, 234, 220)
MUTED = (163, 155, 141)
RUNNING = (255, 138, 61)
STUCK = (255, 103, 103)
READY = (96, 211, 197)
DONE = (141, 229, 168)
ICE = (142, 181, 255)
MEMORY = (243, 203, 122)
ACCENT = (247, 178, 103)


def font(path: str, size: int) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    try:
        return ImageFont.truetype(path, size * SCALE)
    except OSError:
        return ImageFont.load_default()


FONT_SANS = "/System/Library/Fonts/Avenir.ttc"
FONT_DISPLAY = "/System/Library/Fonts/Avenir Next Condensed.ttc"
FONT_MONO = "/System/Library/Fonts/SFNSMono.ttf"


def f_sans(size: int) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    return font(FONT_SANS, size)


def f_display(size: int) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    return font(FONT_DISPLAY, size)


def f_mono(size: int) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    return font(FONT_MONO, size)


def sc(value: int | float) -> int:
    return int(round(value * SCALE))


def box(rect: tuple[int, int, int, int]) -> tuple[int, int, int, int]:
    return tuple(sc(v) for v in rect)


def rgba(color: tuple[int, ...], alpha: int = 255) -> tuple[int, int, int, int]:
    if len(color) == 4:
        alpha = color[3]
        color = color[:3]
    if alpha < 255:
        ratio = alpha / 255
        color = tuple(int(channel * ratio + BG[index] * (1 - ratio)) for index, channel in enumerate(color))
        alpha = 255
    return (color[0], color[1], color[2], alpha)


def make_canvas(width: int, height: int) -> Image.Image:
    image = Image.new("RGBA", (sc(width), sc(height)), rgba(BG))
    draw = ImageDraw.Draw(image, "RGBA")
    for y in range(sc(height)):
        t = y / max(1, sc(height) - 1)
        r = int(18 * (1 - t) + 5 * t)
        g = int(17 * (1 - t) + 8 * t)
        b = int(16 * (1 - t) + 9 * t)
        draw.line((0, y, sc(width), y), fill=(r, g, b, 255))
    for x in range(0, sc(width), sc(32)):
        draw.line((x, 0, x, sc(height)), fill=(255, 255, 255, 8))
    for y in range(0, sc(height), sc(32)):
        draw.line((0, y, sc(width), y), fill=(255, 255, 255, 8))
    draw.ellipse(box((-80, -90, 500, 430)), fill=(96, 211, 197, 26))
    draw.ellipse(box((980, -130, 1520, 390)), fill=(255, 138, 61, 26))
    return image


def rounded(draw: ImageDraw.ImageDraw, rect, radius: int, fill, outline=LINE, width: int = 1) -> None:
    draw.rounded_rectangle(box(rect), radius=sc(radius), fill=rgba(fill), outline=rgba(outline), width=sc(width))


def panel(draw: ImageDraw.ImageDraw, rect, radius: int = 22) -> None:
    rounded(draw, rect, radius, PANEL, LINE, 1)
    x1, y1, x2, _ = rect
    draw.line((sc(x1 + 2), sc(y1 + 2), sc(x2 - 2), sc(y1 + 2)), fill=(255, 255, 255, 18), width=sc(1))


def text(draw: ImageDraw.ImageDraw, xy, body: str, font_obj, fill=TEXT, anchor=None) -> None:
    draw.text((sc(xy[0]), sc(xy[1])), body, font=font_obj, fill=rgba(fill), anchor=anchor)


def text_size(draw: ImageDraw.ImageDraw, body: str, font_obj) -> tuple[int, int]:
    bbox = draw.textbbox((0, 0), body, font=font_obj)
    return ((bbox[2] - bbox[0]) // SCALE, (bbox[3] - bbox[1]) // SCALE)


def wrap(draw: ImageDraw.ImageDraw, body: str, font_obj, width: int) -> list[str]:
    if not body:
        return []
    approx = max(8, width // 8)
    lines: list[str] = []
    for paragraph in body.splitlines() or [""]:
        if not paragraph:
            lines.append("")
            continue
        for line in textwrap.wrap(paragraph, width=approx):
            while text_size(draw, line, font_obj)[0] > width and " " in line:
                parts = line.rsplit(" ", 1)
                lines.append(parts[0])
                line = parts[1]
            lines.append(line)
    return lines


def paragraph(draw: ImageDraw.ImageDraw, xy, body: str, font_obj, width: int, fill=TEXT, line_gap: int = 5) -> int:
    y = xy[1]
    line_height = text_size(draw, "Ag", font_obj)[1] + line_gap
    for line in wrap(draw, body, font_obj, width):
        text(draw, (xy[0], y), line, font_obj, fill)
        y += line_height
    return y


def chip(draw: ImageDraw.ImageDraw, xy, body: str, tone=LINE, fill_alpha: int = 32) -> int:
    font_obj = f_sans(13)
    w, h = text_size(draw, body, font_obj)
    rect = (xy[0], xy[1], xy[0] + w + 20, xy[1] + h + 12)
    rounded(draw, rect, 999, (*tone[:3], fill_alpha), (*tone[:3], 70), 1)
    text(draw, (xy[0] + 10, xy[1] + 5), body, font_obj, (*tone[:3], 255))
    return rect[2]


def node(draw: ImageDraw.ImageDraw, rect, node_id: str, title: str, tone, chips: list[tuple[str, tuple[int, int, int]]]) -> None:
    rounded(draw, rect, 16, (14, 22, 23, 245), (*tone, 160), 2)
    x1, y1, x2, _ = rect
    text(draw, (x1 + 16, y1 + 15), node_id, f_mono(13), MEMORY)
    draw.ellipse(box((x2 - 28, y1 + 18, x2 - 17, y1 + 29)), fill=rgba(tone))
    paragraph(draw, (x1 + 16, y1 + 42), title, f_sans(16), x2 - x1 - 34, TEXT, 4)
    cx = x1 + 16
    cy = y1 + 96
    for label, chip_tone in chips:
        next_x = chip(draw, (cx, cy), label, chip_tone, 34)
        cx = next_x + 7


def arrow(draw: ImageDraw.ImageDraw, start, end, color, dash: bool = False) -> None:
    points = []
    sx, sy = start
    ex, ey = end
    for i in range(40):
        t = i / 39
        c1x = sx + max(60, (ex - sx) * 0.45)
        c2x = ex - max(60, (ex - sx) * 0.45)
        x = (1 - t) ** 3 * sx + 3 * (1 - t) ** 2 * t * c1x + 3 * (1 - t) * t**2 * c2x + t**3 * ex
        y = (1 - t) ** 3 * sy + 3 * (1 - t) ** 2 * t * sy + 3 * (1 - t) * t**2 * ey + t**3 * ey
        points.append((sc(x), sc(y)))
    if dash:
        for i in range(0, len(points) - 1, 4):
            draw.line(points[i : i + 2], fill=rgba(color, 150), width=sc(2))
    else:
        draw.line(points, fill=rgba(color, 150), width=sc(3))


def task_spine() -> None:
    image = make_canvas(1400, 860)
    draw = ImageDraw.Draw(image, "RGBA")
    panel(draw, (30, 28, 1370, 832))
    text(draw, (64, 58), "Task Spine", f_display(42), TEXT)
    text(draw, (64, 103), "Dependency graph with live task state and commit memory.", f_sans(18), MUTED)
    chip(draw, (1100, 62), "All rigs", READY, 48)
    chip(draw, (1188, 62), "Hide completed", MEMORY, 48)
    chip(draw, (1310, 62), "32 nodes", ICE, 48)

    panel(draw, (56, 132, 916, 800), 18)
    panel(draw, (946, 132, 1344, 800), 18)
    for x in range(80, 900, 48):
        draw.line((sc(x), sc(152), sc(x), sc(780)), fill=(255, 255, 255, 8))
    for y in range(160, 785, 48):
        draw.line((sc(76), sc(y), sc(896), sc(y)), fill=(255, 255, 255, 8))

    nodes = {
        "open": (96, 206, 340, 326),
        "run": (388, 190, 632, 322),
        "readme": (680, 186, 884, 298),
        "static": (388, 438, 632, 560),
        "tests": (680, 528, 884, 640),
        "commit": (680, 358, 884, 434),
    }
    arrow(draw, (340, 266), (388, 256), READY)
    arrow(draw, (632, 256), (680, 242), READY)
    arrow(draw, (632, 498), (680, 584), (255, 255, 255), True)
    arrow(draw, (632, 256), (680, 397), MEMORY, True)
    arrow(draw, (340, 266), (388, 498), (255, 255, 255), True)

    node(draw, nodes["open"], "hq-ui-split", "Split dashboard HTML into static assets", READY, [("open", READY), ("frontend", ICE)])
    node(draw, nodes["run"], "hq-renderers", "Abstract Codex and Claude transcript rendering", RUNNING, [("hooked", RUNNING), ("2 agents", READY)])
    node(draw, nodes["readme"], "hq-readme", "Add README visuals and usage notes", READY, [("ready", READY), ("docs", MEMORY)])
    node(draw, nodes["static"], "hq-static", "Serve CSS and ES modules from /static", DONE, [("closed", DONE), ("server", ICE)])
    rounded(draw, nodes["commit"], 999, (12, 18, 19, 245), (*MEMORY, 170), 2)
    text(draw, (704, 381), "9f6c689", f_mono(14), MEMORY)
    text(draw, (704, 405), "Claude renderer commit", f_sans(15), TEXT)
    node(draw, nodes["tests"], "hq-smoke", "Verify terminal API and transcript rendering", ICE, [("deferred", ICE), ("checks", MEMORY)])

    text(draw, (982, 168), "Focus", f_display(34), TEXT)
    text(draw, (982, 210), "hq-renderers", f_mono(15), MEMORY)
    paragraph(draw, (982, 242), "The active task normalizes model transcripts into a shared timeline while preserving provider-specific rendering.", f_sans(18), 314, TEXT)
    chip(draw, (982, 334), "hooked", RUNNING, 48)
    chip(draw, (1070, 334), "claude", READY, 48)
    chip(draw, (1152, 334), "codex", MEMORY, 48)
    text(draw, (982, 410), "Recent Activity", f_sans(18), TEXT)
    for idx, (time_label, body) in enumerate(
        [
            ("14:31", "mayor selected Claude transcript source"),
            ("14:32", "server returned provider=claude"),
            ("14:33", "renderer expanded Bash tool output"),
        ]
    ):
        y = 452 + idx * 74
        rounded(draw, (982, y, 1308, y + 52), 12, (255, 255, 255, 10), (255, 255, 255, 18), 1)
        text(draw, (1000, y + 12), time_label, f_mono(13), MUTED)
        text(draw, (1060, y + 12), body, f_sans(15), TEXT)

    image = image.resize((1400, 860), Image.Resampling.LANCZOS).convert("RGB")
    image.save(OUT / "task-spine.png", quality=94)


def bubble(draw: ImageDraw.ImageDraw, rect, role: str, body: str, tone, markdown: bool = False) -> int:
    x1, y1, x2, _ = rect
    text(draw, (x1, y1), role.upper(), f_sans(12), MUTED)
    text(draw, (x2 - 56, y1), "14:32", f_mono(12), MUTED)
    y = y1 + 22
    rounded(draw, (x1, y, x2, y + rect[3]), 16, (*tone[:3], 24), (*tone[:3], 54), 1)
    if markdown:
        text(draw, (x1 + 18, y + 16), "Triage complete", f_sans(18), TEXT)
        paragraph(draw, (x1 + 18, y + 46), "- Dolt: running with 0s latency\n- Inbox: 0 unread\n- Hook: empty, ready for instructions", f_sans(15), x2 - x1 - 36, TEXT)
    else:
        paragraph(draw, (x1 + 18, y + 16), body, f_sans(15), x2 - x1 - 36, TEXT)
    return y + rect[3] + 18


def tool_row(draw: ImageDraw.ImageDraw, x: int, y: int, w: int, tool_name: str, summary: str, open_output: str | None = None) -> int:
    chip(draw, (x, y), tool_name, MEMORY, 38)
    text(draw, (x + 86, y + 7), summary, f_sans(15), TEXT)
    text(draw, (x + w - 54, y + 7), "14:32", f_mono(12), MUTED)
    y += 38
    if open_output is not None:
        rounded(draw, (x + 22, y, x + w, y + 78), 12, (4, 7, 8, 130), (255, 255, 255, 18), 1)
        paragraph(draw, (x + 38, y + 14), open_output, f_mono(13), w - 54, (216, 210, 198), 5)
        y += 92
    return y + 8


def mayor_chat() -> None:
    image = make_canvas(1400, 860)
    draw = ImageDraw.Draw(image, "RGBA")
    panel(draw, (30, 28, 1370, 832))
    text(draw, (64, 58), "Primary Terminal", f_display(42), TEXT)
    text(draw, (64, 103), "mayor - Claude transcript - HQ", f_sans(18), MUTED)

    panel(draw, (56, 132, 930, 800), 18)
    panel(draw, (958, 132, 1344, 800), 18)

    text(draw, (86, 168), "Mayor Claude", f_sans(22), TEXT)
    text(draw, (86, 198), "mayor - sample-session.jsonl", f_sans(15), MUTED)
    chip(draw, (610, 164), "session live", RUNNING, 44)
    chip(draw, (720, 164), "claude 36 items", MEMORY, 44)
    chip(draw, (842, 164), "just now", ICE, 44)

    rounded(draw, (80, 230, 906, 770), 14, (4, 7, 8, 150), (255, 255, 255, 16), 1)
    y = 254
    y = bubble(draw, (108, y, 874, 82), "user", "Check your hook and mail, then act on hooked work if present.", MEMORY)
    y = tool_row(draw, 108, y, 766, "Bash", "Check hooked work: gt hook", "Hook Status: mayor\nRole: mayor\nNothing on hook - no work slung")
    y = bubble(draw, (108, y, 874, 96), "claude", "No hooked work. I have two fresh escalations in mail. I will read them and check current system health.", READY)
    y = tool_row(draw, 108, y, 766, "Bash", "Read latest escalation: gt mail read hq-52zuy")
    y = tool_row(draw, 108, y, 766, "Bash", "Check Dolt health: gt dolt status", "Dolt server is running\nQuery latency: 0s\nConnections: 4 / 1000")

    text(draw, (994, 168), "Polecats", f_sans(20), TEXT)
    chip(draw, (994, 206), "5 polecats", READY, 38)
    chip(draw, (1096, 206), "2 hooked", RUNNING, 38)
    for idx, (name, state, tone) in enumerate(
        [("jasper", "hooked hq-renderers", RUNNING), ("opal", "idle", READY), ("quartz", "idle", READY)]
    ):
        y2 = 262 + idx * 66
        rounded(draw, (994, y2, 1308, y2 + 48), 12, (255, 255, 255, 10), (255, 255, 255, 18), 1)
        text(draw, (1012, y2 + 12), name, f_sans(16), TEXT)
        text(draw, (1110, y2 + 12), state, f_sans(14), tone)

    text(draw, (994, 500), "Recent Events", f_sans(20), TEXT)
    for idx, body in enumerate(["session_start", "mail read hq-52zuy", "escalation closed"]):
        y2 = 540 + idx * 62
        rounded(draw, (994, y2, 1308, y2 + 46), 12, (255, 255, 255, 10), (255, 255, 255, 18), 1)
        text(draw, (1012, y2 + 12), f"14:{31 + idx:02d}", f_mono(13), MUTED)
        text(draw, (1070, y2 + 12), body, f_sans(15), TEXT)

    image = image.resize((1400, 860), Image.Resampling.LANCZOS).convert("RGB")
    image.save(OUT / "mayor-chat.png", quality=94)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    task_spine()
    mayor_chat()
    print(f"Wrote {OUT / 'task-spine.png'}")
    print(f"Wrote {OUT / 'mayor-chat.png'}")


if __name__ == "__main__":
    main()
