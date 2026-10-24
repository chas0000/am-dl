#!/usr/bin/env python3
"""把一个歌手散落在多个顶层目录里的专辑合并回主目录。

场景：合作专辑的 albumArtist 是「A & B」，早期版本按它建了顶层歌手目录，
结果同一个歌手的唱片散成好几个文件夹（还可能繁简两套）。

用法：
    # 预览（不改任何文件）
    python3 merge-artists.py --root /mnt/disk1/AM-DL --target 罗大佑 \
        --sources "Lo Ta-You" "罗大佑 & 春龙交响乐团" "OK 男女合唱团"

    # 执行，并把 ALBUMARTIST 统一成主歌手
    python3 merge-artists.py --root /mnt/disk1/AM-DL --target 罗大佑 \
        --sources "Lo Ta-You" ... --apply --fix-tags

匹配专辑时用 opencc 繁转简，所以「寶島鹹酸甜」和「宝岛咸酸甜」会认成同一张。
所有移动都记在 <root>/.merge-log.txt，方便回滚。
"""
import argparse
import os
import shutil
import subprocess
import sys

LOG_NAME = ".merge-log.txt"


def t2s(text: str) -> str:
    """繁 → 简；没装 opencc 就原样返回"""
    try:
        p = subprocess.run(
            ["opencc", "-c", "t2s.json"], input=text, capture_output=True, text=True
        )
        return (p.stdout or text).strip() or text
    except FileNotFoundError:
        return text


def album_dirs(path: str) -> list[str]:
    return sorted(
        d for d in os.listdir(path) if os.path.isdir(os.path.join(path, d))
    )


def files_in(path: str) -> list[str]:
    """专辑目录里的文件，递归一层（有的专辑按曲目建子目录）"""
    out = []
    for root, _, names in os.walk(path):
        out += [os.path.join(root, n) for n in names]
    return sorted(out)


def build_plan(root: str, target: str, sources: list[str]) -> list[tuple[str, str, str]]:
    target_dir = os.path.join(root, target)
    index: dict[str, str] = {}
    for name in album_dirs(target_dir):
        index.setdefault(t2s(name), name)

    plan: list[tuple[str, str, str]] = []
    for src in sources:
        src_dir = os.path.join(root, src)
        if not os.path.isdir(src_dir):
            continue
        for album in album_dirs(src_dir):
            album_dir = os.path.join(src_dir, album)
            files = files_in(album_dir)
            if not any(f.endswith(".m4a") for f in files):
                plan.append(("空目录", album_dir, ""))
                continue
            hit = index.get(t2s(album))
            dst_dir = os.path.join(target_dir, hit) if hit else os.path.join(target_dir, album)
            if not hit and not os.path.exists(dst_dir):
                plan.append(("整张", album_dir, dst_dir))
                index[t2s(album)] = album
                continue
            # 逐文件并入：目标里已有同名专辑（或另一个合作目录先移过去了）
            rel_base = len(album_dir) + 1
            for f in files:
                rel = f[rel_base:]
                dst = os.path.join(dst_dir, rel)
                plan.append(("跳过" if os.path.exists(dst) else "并入", f, dst))
    return plan


def apply_plan(root: str, plan: list[tuple[str, str, str]]) -> None:
    log_path = os.path.join(root, LOG_NAME)
    moved = 0
    with open(log_path, "a", encoding="utf-8") as log:
        for action, src, dst in plan:
            if action in ("跳过", "空目录"):
                continue
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.move(src, dst)
            log.write(f"{action}\t{src}\t{dst}\n")
            moved += 1
    print(f"已移动 {moved} 项，记录在 {log_path}")


def cleanup_empty(root: str, sources: list[str]) -> None:
    for src in sources:
        src_dir = os.path.join(root, src)
        if not os.path.isdir(src_dir):
            continue
        for d in album_dirs(src_dir):
            p = os.path.join(src_dir, d)
            for r, dirs, _ in os.walk(p, topdown=False):
                if not os.listdir(r):
                    os.rmdir(r)
        if os.path.isdir(src_dir) and not os.listdir(src_dir):
            os.rmdir(src_dir)
            print(f"删除空目录 {src}")


def fix_tags(root: str, target: str) -> None:
    """把 ALBUMARTIST 统一成主歌手，ARTIST 保持原样"""
    try:
        from mutagen.mp4 import MP4
    except ImportError:
        sys.exit("需要 mutagen：pip install mutagen")

    changed = failed = 0
    for r, _, names in os.walk(os.path.join(root, target)):
        for n in names:
            if not n.endswith(".m4a"):
                continue
            p = os.path.join(r, n)
            try:
                m = MP4(p)
                if (m.tags or {}).get("aART", [""])[0] == target:
                    continue
                m["aART"] = [target]
                m.save()
                changed += 1
            except Exception as e:  # noqa: BLE001
                failed += 1
                print(f"标签写入失败 {p}: {e}")
    print(f"ALBUMARTIST 改为「{target}」：{changed} 首（失败 {failed}）")


def main() -> None:
    ap = argparse.ArgumentParser(description="合并散落的歌手目录")
    ap.add_argument("--root", required=True, help="下载根目录")
    ap.add_argument("--target", required=True, help="主歌手目录名（合并到这里）")
    ap.add_argument("--sources", nargs="+", required=True, help="要合并掉的目录名")
    ap.add_argument("--apply", action="store_true", help="真正执行")
    ap.add_argument("--fix-tags", action="store_true", help="同时统一 ALBUMARTIST")
    args = ap.parse_args()

    if not os.path.isdir(os.path.join(args.root, args.target)):
        sys.exit(f"主目录不存在：{os.path.join(args.root, args.target)}")

    plan = build_plan(args.root, args.target, args.sources)
    counts: dict[str, int] = {}
    for action, _, _ in plan:
        counts[action] = counts.get(action, 0) + 1
    for action, src, dst in plan:
        if action in ("跳过", "空目录"):
            continue
        print(f"{action}  {os.path.relpath(src, args.root)}\n   → {os.path.relpath(dst, args.root)}")
    print(
        "\n合计：并入 {} 个文件、整张 {} 个专辑、跳过 {} 个重名、空目录 {}".format(
            counts.get("并入", 0), counts.get("整张", 0), counts.get("跳过", 0), counts.get("空目录", 0)
        )
    )

    if not args.apply:
        print("（预览模式，加 --apply 执行）")
        return

    apply_plan(args.root, plan)
    cleanup_empty(args.root, args.sources)
    if args.fix_tags:
        fix_tags(args.root, args.target)


if __name__ == "__main__":
    main()
