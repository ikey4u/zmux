from pathlib import Path
import re

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent
SOURCE = ROOT.parent / "SHORTCUT.md"
OUT_ZH = ROOT / "shortcut-quickstart.png"
OUT_EN = ROOT / "shortcut-quickstart-en.png"

WIDTH = 1800
PAGE_PAD = 52
TOP_PAD = 34
COL_GAP = 28
CARD_GAP = 22
CARD_PAD = 24
HEADER_MIN_H = 256
FOOTER_H = 56
CARD_RADIUS = 24

BG_TOP = (5, 15, 32, 255)
BG_BOTTOM = (3, 10, 23, 255)
GRID = (23, 47, 78, 80)
CARD_BG = (8, 20, 39, 246)
CARD_BORDER = (54, 86, 129, 255)
HEADER_BG = (9, 22, 43, 248)
DIVIDER = (55, 74, 102, 255)
TEXT = (236, 241, 247, 255)
MUTED = (148, 168, 194, 255)
WHITE = (255, 255, 255, 255)
SHADOW = (0, 0, 0, 50)

SECTION_TITLE_ZH = {
    "Pane Operations": "Pane 窗格操作",
    "Window Operations": "Window 窗口操作",
    "Session Operations": "Session 会话操作",
    "Command-Line Usage (`zmux` executable)": "命令行启动",
    "Commands supported in command mode (`Prefix + :`)": "命令模式（Prefix + :）",
    "Copy Mode": "Copy Mode 复制模式",
    "Keys Passed Through to the Shell": "默认直通 Shell 的常用键",
    "Notes": "新手提示",
}

SECTION_ACCENTS = {
    "intro": (90, 232, 255, 255),
    "pane": (84, 215, 255, 255),
    "window": (106, 180, 255, 255),
    "session": (255, 176, 77, 255),
    "copy": (212, 108, 255, 255),
    "cli": (84, 237, 198, 255),
    "command": (98, 255, 164, 255),
    "tips": (117, 245, 213, 255),
    "shell": (255, 153, 92, 255),
}

ZH_TEXT = {
    "Split the current pane horizontally into left and right panes": "将当前 pane 水平分成左右两个 pane",
    "Split the current pane vertically into top and bottom panes": "将当前 pane 垂直分成上下两个 pane",
    "Close the current pane": "关闭当前 pane",
    "Maximize the current pane, or restore it when pressed again": "最大化当前 pane；再次按下恢复",
    "Completely clear the current pane output history, including copy mode history": "彻底清空当前 pane 的输出历史，包括 copy mode 历史",
    "Toggle pane borders on or off": "显示或隐藏 pane 边框",
    "Set the current pane's current directory as the working directory for future splits": "将当前 pane 的当前目录设为后续 split 的工作目录",
    "Move focus to the pane on the left, in Vim style": "以 Vim 风格将焦点移动到左侧 pane",
    "Move focus to the pane below, in Vim style": "以 Vim 风格将焦点移动到下方 pane",
    "Move focus to the pane above, in Vim style": "以 Vim 风格将焦点移动到上方 pane",
    "Move focus to the pane on the right, in Vim style": "以 Vim 风格将焦点移动到右侧 pane",
    "Move focus to the pane on the left": "移动到左侧 pane",
    "Move focus to the pane below": "移动到下方 pane",
    "Move focus to the pane above": "移动到上方 pane",
    "Move focus to the pane on the right": "移动到右侧 pane",
    "Resize the active pane left, down, up, or right while Alt/Option remains held. The first Alt/Option+h / j / k / l applies immediately. If there is no resize input for 500 ms, the sequence ends automatically": "按住 Alt/Option 后连续按 h/j/k/l 可调整 pane；500ms 无输入自动结束。",
    "Create a new window": "创建窗口",
    "Switch to the next window": "切到下一个窗口",
    "Switch to the previous window": "切到上一个窗口",
    "Rename the current window, then press Enter to confirm or Esc to cancel": "重命名当前窗口；Enter 确认，Esc 取消",
    "Detach the current client. The server keeps running in the background and all panes stay alive": "detach 当前 client；server 继续运行，pane 保持存活",
    "Rename the current session, then press Enter to confirm or Esc to cancel": "重命名当前 session；Enter 确认，Esc 取消",
    "Switch to the previous session": "切到上一个 session",
    "Switch to the next session": "切到下一个 session",
    "Open the interactive tree view of all sessions and windows. Use Enter to select, j or k to navigate, l to expand, h to collapse, and q or Esc to close": "打开 session / window 树；Enter 选择，j/k 导航，l 展开，h 收起，q/Esc 关闭",
    "Enter command mode. Type a zmux command and press Enter to execute it, or Esc to cancel": "进入命令模式；输入 zmux 命令后 Enter 执行，Esc 取消",
    "Start zmux. If a background server already exists, it attaches automatically": "启动 zmux；若 server 已存在则自动 attach",
    "Attach to an existing background server": "连接已有后台 server",
    "List all current sessions": "列出当前所有 session",
    "Specify the socket name, defaulting to default, to run multiple independent servers at the same time": "指定 socket 名称；可同时运行多个独立 server",
    "Specify the name of the new session": "指定新 session 名称",
    "Start the server in daemon mode. This is usually invoked automatically by zmux and does not need to be run manually": "以 daemon 模式启动 server；通常无需手动执行",
    "Create a new session and switch to it": "创建新 session 并切换过去",
    "Create a new session in the background without switching to it": "后台创建 session，但不切换",
    "Close the current session": "关闭当前 session",
    "Close the specified session": "关闭指定 session",
    "Rename the current session": "重命名当前 session",
    "Switch to the specified session": "切换到指定 session",
    "Rename the current window": "重命名当前窗口",
    "Close the current window": "关闭当前窗口",
    "Split horizontally": "左右分屏",
    "Split vertically": "上下分屏",
    "Maximize or restore the current pane": "最大化或恢复当前 pane",
    "Completely clear the current pane output history": "彻底清空当前 pane 输出历史",
    "Save the current pane's current directory as the working directory for future splits": "保存当前目录，供后续 split 复用",
    "Enter copy mode": "进入复制模式",
    "Exit copy mode": "退出复制模式",
    "Move left, down, up, or right": "移动光标",
    "Move back to the beginning of the current or previous word": "回到当前或上一个单词开头",
    "Move forward to the beginning of the next word": "到下一个单词开头",
    "Move forward to the end of the current or next word": "到当前或下一个单词末尾",
    "Move to the beginning of the line": "跳到行首",
    "Move to the end of the line": "跳到行尾",
    "Jump to the top or bottom": "跳到顶部 / 底部",
    "Scroll up one page": "上一页",
    "Scroll down one page": "下一页",
    "Search forward or backward": "向前 / 向后搜索",
    "Jump to the next or previous search result": "跳到下一个 / 上一个搜索结果",
    "Start character selection": "开始字符选择",
    "Start line selection": "开始整行选择",
    "Start rectangular selection": "开始矩形选择",
    "Copy the current selection and exit copy mode": "复制所选内容并退出复制模式",
    "Send a literal Ctrl+a, which usually moves to the beginning of the line in shell editing": "发送原始 Ctrl+a；常用于回到行首",
    "Move backward by one character": "向左移动一个字符",
    "Interrupt the current foreground process with SIGINT": "发送 SIGINT 中断前台进程",
    "Delete the character under the cursor. On an empty line, it usually means EOF": "删除当前字符；空行时通常表示 EOF",
    "Move to the end of the line in shell editing": "移动到行尾",
    "Move forward by one character": "向右移动一个字符",
    "Delete to the end of the line": "删除到行尾",
    "Clear the screen": "清屏",
    "Go to the next history entry": "下一条历史记录",
    "Go to the previous history entry": "上一条历史记录",
    "Search command history backward incrementally": "向后增量搜索历史",
    "Search command history forward incrementally": "向前增量搜索历史",
    "Transpose the two characters around the cursor": "交换光标附近两个字符",
    "Delete to the beginning of the line": "删除到行首",
    "Suspend the current foreground process with SIGTSTP": "发送 SIGTSTP 挂起前台进程",
    "Pass through to the shell unchanged": "原样传给 shell",
    "Type exit or press Ctrl+d inside a pane to close that pane.": "在 pane 内输入 exit 或按 Ctrl+d 可关闭该 pane。",
    "After the last pane is closed, the server daemon exits automatically and the client exits with it.": "最后一个 pane 关闭后，server 与 client 会自动退出。",
    "Prefix + d only detaches the current client. The server and all panes continue running in the background, and you can reconnect with zmux a.": "Prefix + d 只会 detach 当前 client；server 和 pane 仍在后台运行，可用 zmux a 重连。",
    "If you press the prefix key and do not follow it with an action key, prefix mode stays active until the next key press.": "按下 prefix 后若没有继续动作键，prefix 会保持到下一次按键。",
    "The prefix key itself will be configurable through a config file in the future.": "未来会支持通过配置文件自定义 prefix 键。",
    "Mouse support and more configurable key bindings will be improved in future versions, and this document will be updated accordingly.": "后续会继续完善鼠标支持与更灵活的快捷键配置。",
}

LOCALE_META = {
    "zh": {
        "header_label": "SIGNAL ATLAS / QUICK START",
        "header_title": "ZMUX",
        "header_subtitle": "快捷键速查图",
        "header_desc": "先记住 Prefix，再记住 Pane / Window / Session。",
        "prefix_title": "PREFIX",
        "prefix_value": "Ctrl+a",
        "prefix_desc": "先按前缀，再按动作键",
        "footer": "基于 docs/SHORTCUT.md 生成 · 更新文档后重新运行 docs/diagrams/generate_shortcut_quickstart.py",
        "intro_title": "先记住 Prefix",
        "intro_subtitle": "所有快捷键都从这里开始",
        "intro_bullets": [
            "所有快捷键都要先按 Prefix，再按动作键",
            "连续按两次 Ctrl+a，会发送原始 Ctrl+a 到当前 pane",
            "如果按下 Prefix 后没有动作键，Prefix 会保持到下一次按键",
            "先只记住 Pane / Window / Session 三类高频操作",
        ],
        "pane_subtitle": "分屏、切焦点、缩放",
        "window_subtitle": "创建、切换、重命名",
        "session_subtitle": "detach、切换、查看、命令入口",
        "copy_subtitle": "滚动、搜索、选择、复制",
        "cli_subtitle": "直接在 shell 里调用 zmux",
        "command_subtitle": "命令模式里最常用的一组命令",
        "tips_title": "新手提示",
        "tips_subtitle": "理解 detach 和 close 的区别",
        "shell_subtitle": "这些键不会被 zmux 拦截",
    },
    "en": {
        "header_label": "SIGNAL ATLAS / QUICK START",
        "header_title": "ZMUX",
        "header_subtitle": "Shortcut Quickstart",
        "header_desc": "Memorize Prefix first, then Pane, Window, and Session basics.",
        "prefix_title": "PREFIX",
        "prefix_value": "Ctrl+a",
        "prefix_desc": "Press Prefix first, then the action key",
        "footer": "Generated from docs/SHORTCUT.md · Re-run docs/diagrams/generate_shortcut_quickstart.py after updates",
        "intro_title": "Memorize Prefix first",
        "intro_subtitle": "Every shortcut starts here",
        "intro_bullets": [
            "Every zmux shortcut begins with Prefix, then the action key",
            "Press Ctrl+a twice to send a literal Ctrl+a into the current pane",
            "If you stop after Prefix, prefix mode stays active until the next key",
            "Start by memorizing the Pane, Window, and Session groups only",
        ],
        "pane_subtitle": "Split, move focus, resize",
        "window_subtitle": "Create, switch, rename",
        "session_subtitle": "Detach, switch, inspect, command entry",
        "copy_subtitle": "Scroll, search, select, copy",
        "cli_subtitle": "Run zmux directly from the shell",
        "command_subtitle": "The commands most people use first",
        "tips_title": "Beginner tips",
        "tips_subtitle": "Know the difference between detach and close",
        "shell_subtitle": "These keys pass straight to the shell",
    },
}


def find_font(candidates):
    for candidate in candidates:
        path = Path(candidate)
        if path.exists():
            return str(path)
    return None


SANS_PATH = find_font(
    [
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
    ]
)
MONO_PATH = find_font(
    [
        "/System/Library/Fonts/Supplemental/Menlo.ttc",
        "/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Monaco.ttf",
    ]
)


def load_font(size, mono=False):
    path = MONO_PATH if mono else SANS_PATH
    if path:
        return ImageFont.truetype(path, size)
    return ImageFont.load_default()


FONTS = {
    "label": load_font(18, mono=True),
    "title": load_font(70, mono=True),
    "subtitle": load_font(28),
    "body": load_font(18),
    "body_small": load_font(16),
    "section": load_font(24),
    "section_sub": load_font(15),
    "key": load_font(17, mono=True),
    "prefix_title": load_font(16, mono=True),
    "prefix_key": load_font(34, mono=True),
}

DUMMY = ImageDraw.Draw(Image.new("RGBA", (WIDTH, 400), (0, 0, 0, 0)))


def clean_text(text):
    text = text.replace("`", "")
    text = re.sub(r"\s+", " ", text)
    return text.strip()



def parse_table(lines):
    rows = []
    for index, line in enumerate(lines):
        cells = [clean_text(cell) for cell in line.strip().strip("|").split("|")]
        if index == 0:
            continue
        if re.fullmatch(r"[-:\s]+", "".join(cells)):
            continue
        if len(cells) >= 2:
            rows.append((cells[0], cells[1]))
    return rows



def parse_shortcut_markdown(text):
    sections = {}
    current = None
    current_subsection = None
    lines = text.splitlines()
    index = 0
    while index < len(lines):
        line = lines[index].rstrip()
        if line.startswith("## "):
            current = line[3:].strip()
            sections[current] = {
                "paragraphs": [],
                "bullets": [],
                "quotes": [],
                "tables": [],
                "subsections": {},
            }
            current_subsection = None
            index += 1
            continue
        if current is None:
            index += 1
            continue
        if line.startswith("### "):
            current_subsection = line[4:].strip()
            sections[current]["subsections"][current_subsection] = {
                "paragraphs": [],
                "bullets": [],
                "quotes": [],
                "tables": [],
            }
            index += 1
            continue
        target = sections[current]
        if current_subsection is not None:
            target = sections[current]["subsections"][current_subsection]
        if line.startswith("|"):
            block = []
            while index < len(lines) and lines[index].startswith("|"):
                block.append(lines[index].rstrip())
                index += 1
            target["tables"].append(parse_table(block))
            continue
        if line.startswith(">"):
            target["quotes"].append(clean_text(line[1:]))
        elif line.startswith("- "):
            target["bullets"].append(clean_text(line[2:]))
        elif line.strip():
            target["paragraphs"].append(clean_text(line))
        index += 1
    return sections



def contains_cjk(text):
    return bool(re.search(r"[\u4e00-\u9fff]", text))



def measure(text, font, spacing=6):
    if not text:
        return 0, 0
    box = DUMMY.multiline_textbbox((0, 0), text, font=font, spacing=spacing)
    return box[2] - box[0], box[3] - box[1]



def split_token(token, font, width):
    if measure(token, font)[0] <= width:
        return [token]
    parts = []
    current = ""
    for char in token:
        candidate = current + char
        if current and measure(candidate, font)[0] > width:
            parts.append(current)
            current = char
        else:
            current = candidate
    if current:
        parts.append(current)
    return parts



def wrap_text(text, font, width):
    text = text.strip()
    if not text:
        return ""
    if contains_cjk(text):
        tokens = list(text)
    else:
        tokens = re.findall(r"\S+\s*", text) or [text]
    final_tokens = []
    for token in tokens:
        stripped = token.rstrip()
        if stripped and measure(stripped, font)[0] > width:
            parts = split_token(stripped, font, width)
            if token.endswith(" ") and parts:
                parts[-1] += " "
            final_tokens.extend(parts)
        else:
            final_tokens.append(token)
    lines = []
    current = ""
    for token in final_tokens:
        candidate = current + token
        if not current:
            current = token
            continue
        if measure(candidate.rstrip(), font)[0] <= width:
            current = candidate
        else:
            lines.append(current.rstrip())
            current = token.lstrip() if not contains_cjk(text) else token
    if current:
        lines.append(current.rstrip())
    return "\n".join(lines)



def text_h(text, font, spacing=6):
    return measure(text, font, spacing)[1]



def translate(text, locale):
    if locale == "en":
        return text
    return ZH_TEXT.get(text, text)



def section_title(title, locale):
    if locale == "en":
        if title == "Command-Line Usage (`zmux` executable)":
            return "Command line"
        if title == "Commands supported in command mode (`Prefix + :`)":
            return "Command mode (Prefix + :)"
        return clean_text(title)
    return SECTION_TITLE_ZH.get(title, clean_text(title))



def luminance(color):
    r, g, b = color[:3]
    return 0.299 * r + 0.587 * g + 0.114 * b



def key_text_color(accent):
    return BG_TOP if luminance(accent) > 150 else WHITE



def draw_background(draw, height):
    for y in range(height):
        t = y / max(1, height - 1)
        color = tuple(int(BG_TOP[i] + (BG_BOTTOM[i] - BG_TOP[i]) * t) for i in range(4))
        draw.line((0, y, WIDTH, y), fill=color)
    step = 72
    for x in range(0, WIDTH, step):
        draw.line((x, 0, x, height), fill=GRID, width=1)
    for y in range(0, height, step):
        draw.line((0, y, WIDTH, y), fill=GRID, width=1)



def draw_card_base(draw, bounds, fill=CARD_BG, outline=CARD_BORDER):
    x1, y1, x2, y2 = bounds
    draw.rounded_rectangle((x1, y1 + 8, x2, y2 + 8), radius=CARD_RADIUS, fill=SHADOW)
    draw.rounded_rectangle(bounds, radius=CARD_RADIUS, fill=fill, outline=outline)



def draw_section_header(draw, x, y, width, title, subtitle, accent):
    draw.rounded_rectangle((x, y, x + 56, y + 6), radius=3, fill=accent)
    draw.text((x, y + 18), title, font=FONTS["section"], fill=WHITE)
    cursor = y + 18 + text_h(title, FONTS["section"], 6)
    if subtitle:
        wrapped = wrap_text(subtitle, FONTS["section_sub"], width)
        draw.multiline_text((x, cursor + 4), wrapped, font=FONTS["section_sub"], fill=MUTED, spacing=5)
        cursor += 4 + text_h(wrapped, FONTS["section_sub"], 5)
    cursor += 12
    draw.line((x, cursor, x + width, cursor), fill=DIVIDER, width=1)
    return cursor + 12



def row_metrics(key, value, width, font):
    key_w = 182
    value_w = width - key_w - 16
    key_wrapped = wrap_text(key, FONTS["key"], key_w - 22)
    value_wrapped = wrap_text(value, font, value_w)
    key_h = text_h(key_wrapped, FONTS["key"], 5)
    value_h = text_h(value_wrapped, font, 6)
    row_h = max(34, max(key_h + 14, value_h + 6))
    return key_w, key_wrapped, value_wrapped, row_h



def table_card_height(card, width):
    inner = width - CARD_PAD * 2
    cursor = CARD_PAD
    cursor = drawless_section_height(card["title"], card["subtitle"], inner)
    for key, value in card["rows"]:
        _, _, _, row_h = row_metrics(key, value, inner, FONTS["body_small"])
        cursor += row_h + 8
    return cursor + CARD_PAD - 8



def bullet_item_height(text, width):
    wrapped = wrap_text(text, FONTS["body"], width - 26)
    return max(22, text_h(wrapped, FONTS["body"], 7))



def bullets_card_height(card, width):
    inner = width - CARD_PAD * 2
    cursor = drawless_section_height(card["title"], card["subtitle"], inner)
    for item in card["items"]:
        cursor += bullet_item_height(item, inner) + 12
    return cursor + CARD_PAD - 12



def drawless_section_height(title, subtitle, width):
    value = 18 + text_h(title, FONTS["section"], 6)
    if subtitle:
        wrapped = wrap_text(subtitle, FONTS["section_sub"], width)
        value += 4 + text_h(wrapped, FONTS["section_sub"], 5)
    return value + 24



def draw_table_card(draw, x, y, width, card):
    height = table_card_height(card, width)
    draw_card_base(draw, (x, y, x + width, y + height))
    inner_x = x + CARD_PAD
    inner_w = width - CARD_PAD * 2
    cursor = y + CARD_PAD
    cursor = draw_section_header(draw, inner_x, cursor, inner_w, card["title"], card["subtitle"], card["accent"])
    for key, value in card["rows"]:
        key_w, key_wrapped, value_wrapped, row_h = row_metrics(key, value, inner_w, FONTS["body_small"])
        pill_y = cursor + max(0, (row_h - max(28, text_h(key_wrapped, FONTS["key"], 5) + 10)) / 2)
        pill_h = max(28, text_h(key_wrapped, FONTS["key"], 5) + 10)
        draw.rounded_rectangle((inner_x, pill_y, inner_x + key_w, pill_y + pill_h), radius=12, fill=card["accent"])
        draw.multiline_text((inner_x + 11, pill_y + 6), key_wrapped, font=FONTS["key"], fill=key_text_color(card["accent"]), spacing=5)
        draw.multiline_text((inner_x + key_w + 16, cursor), value_wrapped, font=FONTS["body_small"], fill=TEXT, spacing=6)
        cursor += row_h + 8
    return height



def draw_bullets_card(draw, x, y, width, card):
    height = bullets_card_height(card, width)
    draw_card_base(draw, (x, y, x + width, y + height))
    inner_x = x + CARD_PAD
    inner_w = width - CARD_PAD * 2
    cursor = y + CARD_PAD
    cursor = draw_section_header(draw, inner_x, cursor, inner_w, card["title"], card["subtitle"], card["accent"])
    for item in card["items"]:
        wrapped = wrap_text(item, FONTS["body"], inner_w - 28)
        item_h = bullet_item_height(item, inner_w)
        dot_y = cursor + 7
        draw.ellipse((inner_x, dot_y, inner_x + 10, dot_y + 10), fill=card["accent"])
        draw.multiline_text((inner_x + 18, cursor), wrapped, font=FONTS["body"], fill=TEXT, spacing=7)
        cursor += item_h + 12
    return height



def header_height(locale):
    meta = LOCALE_META[locale]
    width = WIDTH - PAGE_PAD * 2
    prefix_w = 286
    prefix_h = 126
    side_pad = 26
    gap = 28
    left_w = width - side_pad * 2 - prefix_w - gap
    desc = wrap_text(meta["header_desc"], FONTS["body"], left_w)
    cursor = 22
    cursor += text_h(meta["header_label"], FONTS["label"], 4)
    cursor += 24
    cursor += text_h(meta["header_title"], FONTS["title"], 4)
    cursor += 18
    cursor += text_h(meta["header_subtitle"], FONTS["subtitle"], 4)
    cursor += 16
    cursor += text_h(desc, FONTS["body"], 6)
    content_h = cursor + 34
    prefix_total_h = 24 + prefix_h + 34
    return max(HEADER_MIN_H, content_h, prefix_total_h)


def draw_header(draw, locale, header_h):
    meta = LOCALE_META[locale]
    x = PAGE_PAD
    y = TOP_PAD
    width = WIDTH - PAGE_PAD * 2
    side_pad = 26
    prefix_w = 286
    prefix_h = 126
    gap = 28
    left_x = x + side_pad
    left_w = width - side_pad * 2 - prefix_w - gap
    draw_card_base(draw, (x, y, x + width, y + header_h), fill=HEADER_BG, outline=CARD_BORDER)
    cursor = y + 22
    draw.text((left_x, cursor), meta["header_label"], font=FONTS["label"], fill=MUTED)
    cursor += text_h(meta["header_label"], FONTS["label"], 4) + 24
    draw.text((left_x, cursor), meta["header_title"], font=FONTS["title"], fill=WHITE)
    cursor += text_h(meta["header_title"], FONTS["title"], 4) + 18
    draw.text((left_x, cursor), meta["header_subtitle"], font=FONTS["subtitle"], fill=TEXT)
    cursor += text_h(meta["header_subtitle"], FONTS["subtitle"], 4) + 16
    desc = wrap_text(meta["header_desc"], FONTS["body"], left_w)
    draw.multiline_text((left_x, cursor), desc, font=FONTS["body"], fill=MUTED, spacing=6)
    px = x + width - prefix_w - 24
    py = y + 24
    draw.rounded_rectangle((px, py, px + prefix_w, py + prefix_h), radius=18, fill=(14, 31, 59, 255), outline=(86, 136, 196, 255))
    draw.text((px + 18, py + 16), meta["prefix_title"], font=FONTS["prefix_title"], fill=MUTED)
    draw.text((px + 18, py + 42), meta["prefix_value"], font=FONTS["prefix_key"], fill=WHITE)
    draw.text((px + 18, py + 92), meta["prefix_desc"], font=FONTS["body_small"], fill=MUTED)
    draw.line((x + 26, y + header_h - 26, x + width - 26, y + header_h - 26), fill=DIVIDER, width=1)



def build_tips_items(parsed, locale):
    shell = parsed["Keys Passed Through to the Shell"]
    notes = parsed["Notes"]
    items = [
        translate(shell["quotes"][0], locale),
        translate(shell["quotes"][2], locale),
        translate(shell["quotes"][1], locale),
        translate(notes["bullets"][0], locale),
    ]
    return items



def build_shell_items(parsed, locale):
    rows = dict(parsed["Keys Passed Through to the Shell"]["tables"][0])
    selected = [
        "Ctrl+a Ctrl+a",
        "Ctrl+c",
        "Ctrl+d",
        "Ctrl+b",
        "Ctrl+l",
        "Ctrl+r",
    ]
    items = []
    for key in selected:
        items.append(f"{key}：{translate(rows[key], locale)}")
    return items



def build_cards(parsed, locale):
    meta = LOCALE_META[locale]
    cli_section = parsed["Command-Line Usage (`zmux` executable)"]
    cmd_section = cli_section["subsections"]["Commands supported in command mode (`Prefix + :`)"]
    left_cards = [
        {
            "kind": "bullets",
            "title": meta["intro_title"],
            "subtitle": meta["intro_subtitle"],
            "accent": SECTION_ACCENTS["intro"],
            "items": meta["intro_bullets"],
        },
        {
            "kind": "table",
            "title": section_title("Pane Operations", locale),
            "subtitle": meta["pane_subtitle"],
            "accent": SECTION_ACCENTS["pane"],
            "rows": [(key, translate(value, locale)) for key, value in parsed["Pane Operations"]["tables"][0]],
        },
        {
            "kind": "table",
            "title": section_title("Window Operations", locale),
            "subtitle": meta["window_subtitle"],
            "accent": SECTION_ACCENTS["window"],
            "rows": [(key, translate(value, locale)) for key, value in parsed["Window Operations"]["tables"][0]],
        },
        {
            "kind": "bullets",
            "title": meta["tips_title"],
            "subtitle": meta["tips_subtitle"],
            "accent": SECTION_ACCENTS["tips"],
            "items": build_tips_items(parsed, locale),
        },
    ]
    right_cards = [
        {
            "kind": "table",
            "title": section_title("Session Operations", locale),
            "subtitle": meta["session_subtitle"],
            "accent": SECTION_ACCENTS["session"],
            "rows": [(key, translate(value, locale)) for key, value in parsed["Session Operations"]["tables"][0]],
        },
        {
            "kind": "table",
            "title": section_title("Copy Mode", locale),
            "subtitle": meta["copy_subtitle"],
            "accent": SECTION_ACCENTS["copy"],
            "rows": [(key, translate(value, locale)) for key, value in parsed["Copy Mode"]["tables"][0]],
        },
        {
            "kind": "table",
            "title": section_title("Command-Line Usage (`zmux` executable)", locale),
            "subtitle": meta["cli_subtitle"],
            "accent": SECTION_ACCENTS["cli"],
            "rows": [(key, translate(value, locale)) for key, value in cli_section["tables"][0]],
        },
        {
            "kind": "table",
            "title": section_title("Commands supported in command mode (`Prefix + :`)", locale),
            "subtitle": meta["command_subtitle"],
            "accent": SECTION_ACCENTS["command"],
            "rows": [(key, translate(value, locale)) for key, value in cmd_section["tables"][0]],
        },
        {
            "kind": "bullets",
            "title": section_title("Keys Passed Through to the Shell", locale),
            "subtitle": meta["shell_subtitle"],
            "accent": SECTION_ACCENTS["shell"],
            "items": build_shell_items(parsed, locale),
        },
    ]
    return left_cards, right_cards



def draw_card(draw, x, y, width, card):
    if card["kind"] == "table":
        return draw_table_card(draw, x, y, width, card)
    return draw_bullets_card(draw, x, y, width, card)



def render_locale(locale, parsed):
    left_cards, right_cards = build_cards(parsed, locale)
    header_h = header_height(locale)
    col_w = int((WIDTH - PAGE_PAD * 2 - COL_GAP) / 2)
    left_y = TOP_PAD + header_h + CARD_GAP
    right_y = TOP_PAD + header_h + CARD_GAP
    for card in left_cards:
        h = table_card_height(card, col_w) if card["kind"] == "table" else bullets_card_height(card, col_w)
        left_y += h + CARD_GAP
    for card in right_cards:
        h = table_card_height(card, col_w) if card["kind"] == "table" else bullets_card_height(card, col_w)
        right_y += h + CARD_GAP
    height = max(left_y, right_y) + FOOTER_H
    image = Image.new("RGBA", (WIDTH, height), BG_TOP)
    draw = ImageDraw.Draw(image)
    draw_background(draw, height)
    draw_header(draw, locale, header_h)
    left_x = PAGE_PAD
    right_x = PAGE_PAD + col_w + COL_GAP
    left_cursor = TOP_PAD + header_h + CARD_GAP
    right_cursor = TOP_PAD + header_h + CARD_GAP
    for card in left_cards:
        left_cursor += draw_card(draw, left_x, left_cursor, col_w, card) + CARD_GAP
    for card in right_cards:
        right_cursor += draw_card(draw, right_x, right_cursor, col_w, card) + CARD_GAP
    draw.text((PAGE_PAD, height - 34), LOCALE_META[locale]["footer"], font=FONTS["body_small"], fill=MUTED)
    output = OUT_ZH if locale == "zh" else OUT_EN
    image.convert("RGB").save(output)
    return output, image.size



def main():
    parsed = parse_shortcut_markdown(SOURCE.read_text(encoding="utf-8"))
    for locale in ("zh", "en"):
        path, size = render_locale(locale, parsed)
        print(f"generated {path.name} {size[0]}x{size[1]}")


if __name__ == "__main__":
    main()
