"""Resolve the app's game catalogue (app/src/games.rs display names) against Discord's
detectable-applications list and writes the manifest of icons to embed.

Usage: python tools/game_icons/manifest.py <games.rs> <detectable.json> <manifest.json>

The matching mirrors game_icons.rs: exact normalised name/alias first, then a prefix
relation in either direction (shortest entry name wins, first breaks ties). Only entries
with an icon_hash are used - an entry without one has no art to download.
"""
import json
import re
import sys


def key(s: str) -> str:
    return re.sub(r'[^a-z0-9]', '', s.lower())


def main() -> None:
    games_src, detectable, out = sys.argv[1], sys.argv[2], sys.argv[3]
    src = open(games_src, encoding='utf-8').read()
    names = sorted({n for _, n in re.findall(r'\("([a-z0-9 .+\-]+)",\s*"([^"]+)"\)', src)})
    if not names:
        raise SystemExit('nenhum nome de jogo encontrado no catalogo')

    data = json.load(open(detectable, encoding='utf-8'))
    entries = []
    for e in data:
        icon = e.get('icon_hash')
        if not icon:
            continue
        ks = []
        if e.get('name'):
            ks.append(key(e['name']))
        for a in (e.get('aliases') or []):
            if a and key(a) not in ks:
                ks.append(key(a))
        entries.append((e['id'], icon, ks, e.get('name')))

    manifest, missing = [], []
    for name in names:
        k = key(name)
        hit = next((e for e in entries if k in e[2]), None)
        if hit is None:
            prefix = None
            for app, icon, ks, label in entries:
                for n in ks:
                    if len(n) < 4 or len(k) < 4:
                        continue
                    if k.startswith(n) or n.startswith(k):
                        if prefix is None or len(prefix[2][0]) > len(n):
                            prefix = (app, icon, ks, label)
            hit = prefix
        if hit:
            manifest.append({'name': name, 'key': k, 'app': hit[0], 'icon': hit[1], 'label': hit[3]})
        else:
            missing.append(name)

    json.dump(manifest, open(out, 'w', encoding='utf-8'), ensure_ascii=False, indent=1)
    print(f'{len(manifest)} icones resolvidos, {len(missing)} sem icone: {missing}')


if __name__ == '__main__':
    main()