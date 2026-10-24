#!/usr/bin/env python3
"""把专辑目录名里的音质标签去掉，并合并同专辑的多个音质版本。

`罗大佑/安可曲 (2022) [ALAC 16bit-48kHz]/`  →  `罗大佑/安可曲 (2022)/`

同一张专辑存在多个音质目录时，逐曲目保留码率最高的那份，
其余的移到 `<root>/.trash/<时间戳>/`（不直接删，确认后自己清）。

用法：
    python3 normalize-albums.py --root /mnt/disk1/AM-DL            # 预览
    python3 normalize-albums.py --root /mnt/disk1/AM-DL --apply    # 执行
"""
import argparse
import os
import re
import shutil
import subprocess
import sys
import time

QUALITY_RE = re.compile(r"\s*\[[^\[\]]*\]\s*$")


def t2s(text: str) -> str:
    try:
        p = subprocess.run(
            ["opencc", "-c", "t2s.json"], input=text, capture_output=True, text=True
        )
        return (p.stdout or text).strip() or text
    except FileNotFoundError:
        return text


def quality_of(name: str) -> str:
    m = QUALITY_RE.search(name)
    return m.group(0).strip()[1:-1] if m else ""


def strip_quality(name: str) -> str:
    return QUALITY_RE.sub("", name).strip()


def score(quality: str) -> int:
    """音质标签打分，越高越好"""
    q = quality.lower()
    if "alac" in q:
        bits = re.search(r"(\d+)bit", q)
        rate = re.search(r"-([\d.]+)khz", q)
        b = int(bits.group(1)) if bits else 16
        r = float(rate.group(1)) if rate else 44.1
        return 3000 + b * 10 + int(r)
    if "atmos" in q:
        return 2500
    if "binaural" in q:
        return 2200
    m = re.search(r"aac\s*(\d+)", q)
    if m:
        return 2000 + int(m.group(1))
    if "he-aac" in q:
        return 1800
    if "downmix" in q:
        return 1700
    return 1000


def year_of(name: str) -> str:
    m = re.search(r"\((\d{4})\)", name)
    return m.group(1) if m else ""


TRACK_NO_RE = re.compile(r"^\d{1,3}[\s._-]+")


def track_key(filename: str) -> str:
    """同一首歌的识别键：去掉曲目号前缀和扩展名，再做繁简归一。

    只用曲目号不行——不同版本（EP / 专辑 / 不同发行）的同一轨号可能是不同的歌；
    只用文件名也不行——同一首歌在不同音质目录里可能带不同的轨号前缀。
    """
    stem = TRACK_NO_RE.sub("", os.path.splitext(filename)[0])
    return t2s(stem).strip().lower()


def album_dirs(path: str) -> list[str]:
    return sorted(d for d in os.listdir(path) if os.path.isdir(os.path.join(path, d)))


def m4a_files(path: str) -> list[str]:
    out = []
    for root, _, names in os.walk(path):
        out += [os.path.join(root, n) for n in names if n.endswith(".m4a")]
    return sorted(out)


def main() -> None:
    ap = argparse.ArgumentParser(description="去掉专辑目录里的音质标签并合并音质版本")
    ap.add_argument("--root", required=True)
    ap.add_argument("--apply", action="store_true")
    args = ap.parse_args()

    root = args.root
    trash = os.path.join(root, ".trash", time.strftime("%Y%m%d-%H%M%S"))
    renames: list[tuple[str, str]] = []
    renames_file: list[tuple[str, str]] = []
    losers: list[tuple[str, str]] = []  # (源文件, 音质说明)
    moves: list[tuple[str, str]] = []  # 保留但需要换目录的文件

    for artist in album_dirs(root):
        if artist.startswith("."):
            continue
        artist_dir = os.path.join(root, artist)
        groups: dict[tuple[str, str], list[str]] = {}
        for album in album_dirs(artist_dir):
            key = (t2s(strip_quality(album)), year_of(album))
            groups.setdefault(key, []).append(album)

        for (base, _year), albums in sorted(groups.items()):
            # 目标目录名：优先用已经是简体的那个（繁简两套时合并成简体名）
            target_name = sorted(
                albums,
                key=lambda a: (t2s(strip_quality(a)) == strip_quality(a), score(quality_of(a))),
                reverse=True,
            )[0]
            target_name = strip_quality(target_name)
            target_dir = os.path.join(artist_dir, target_name)

            # 每个音质目录里按曲目号分组
            per_track: dict[str, list[tuple[int, str, str]]] = {}
            for album in albums:
                q = quality_of(album)
                for f in m4a_files(os.path.join(artist_dir, album)):
                    per_track.setdefault(track_key(os.path.basename(f)), []).append(
                        (score(q), f, album)
                    )

            for key, cands in sorted(per_track.items()):
                cands.sort(reverse=True)
                keep = cands[0][1]
                # 被淘汰那份的文件名轨号更准（如 05 草螟弄鸡公 vs 01 草螟弄鸡公），保留那份改名
                if len(cands) > 1:
                    better_name = os.path.basename(cands[1][1])
                    if better_name != os.path.basename(keep):
                        renames_file.append((keep, os.path.join(os.path.dirname(keep), better_name)))
                for _s, f, _album in cands[1:]:
                    losers.append((f, os.path.relpath(f, root)))
                    # 歌词/字幕跟着音频一起走，避免留下孤儿文件
                    stem = os.path.splitext(f)[0]
                    for ext in (".lrc", ".ttml", ".txt"):
                        side = stem + ext
                        if os.path.exists(side):
                            losers.append((side, os.path.relpath(side, root)))

            for album in albums:
                src = os.path.join(artist_dir, album)
                if os.path.abspath(src) != os.path.abspath(target_dir):
                    renames.append((src, target_dir))

    print(f"专辑目录改名/合并：{len(set(renames))} 项")
    for src, dst in sorted(set(renames))[:40]:
        print(f"  {os.path.relpath(src, root)}\n   → {os.path.relpath(dst, root)}")

    print(f"淘汰（移入回收站）的文件：{len(losers)} 个")
    for f, rel in losers[:20]:
        print(f"  ✗ {rel}")
    if len(losers) > 20:
        print(f"  … 另外 {len(losers) - 20} 个")

    if not args.apply:
        print("\n（预览模式，加 --apply 执行）")
        return

    # 0) 保留的那份改用更准的文件名（轨号）
    for src, dst in renames_file:
        if not os.path.exists(dst):
            os.rename(src, dst)
    # 1) 先把淘汰的搬走
    for f, rel in losers:
        dst = os.path.join(trash, rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        shutil.move(f, dst)
    # 2) 目录改名：先把旧的整个并进目标，再删空目录
    for src, dst in sorted(set(renames), key=lambda x: -len(x[0])):
        if os.path.abspath(src) == os.path.abspath(dst):
            continue
        for root_dir, _, names in os.walk(src):
            for n in names:
                sp = os.path.join(root_dir, n)
                rel = os.path.relpath(sp, src)
                dp = os.path.join(dst, rel)
                if os.path.exists(dp):
                    continue
                os.makedirs(os.path.dirname(dp), exist_ok=True)
                shutil.move(sp, dp)
    # 3) 清空目录
    for artist in album_dirs(root):
        artist_dir = os.path.join(root, artist)
        if artist.startswith(".") or not os.path.isdir(artist_dir):
            continue
        for album in album_dirs(artist_dir):
            p = os.path.join(artist_dir, album)
            for r, _dirs, _names in os.walk(p, topdown=False):
                if not os.listdir(r):
                    os.rmdir(r)

    log_path = os.path.join(root, ".normalize-log.txt")
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(f"# {time.strftime('%F %T')}\n")
        for src, dst in sorted(set(renames)):
            log.write(f"rename\t{src}\t{dst}\n")
        for src, dst in renames_file:
            log.write(f"file\t{src}\t{dst}\n")
        for f, rel in losers:
            log.write(f"trash\t{f}\t{os.path.join(trash, rel)}\n")
    print(f"\n完成。淘汰的文件在 {trash}，改名记录在 {log_path}")


if __name__ == "__main__":
    main()
