"""Helpers for tools/verify-8bf.sh: unpacking, fixtures, pixel checks.

Kept out of the shell script because the checks need real parsing, and
because a heredoc inside a heredoc is nobody's friend.
"""

import os
import sys
import zipfile


def extract():
    """Pull the plug-in binaries out of the downloaded archives.

    G'MIC needs its companion tree beside it or it stops at Start with
    its own "resources missing" code, so that one is extracted whole.
    """
    zf = zipfile.ZipFile('ff.zip')
    for name in zf.namelist():
        for want in ('FilterFoundry64.8bf', 'FilterFoundry.8bf'):
            if name.endswith('/' + want) or name == want:
                open(want, 'wb').write(zf.read(name))
    zipfile.ZipFile('gm.zip').extractall('.')


FT_REPO = 'rechmbrs/FtPattern'
FT_FILES = [
    'FtWinPlugins08oct2019/Ft2DF.8bf',
    'FtWinPlugins08oct2019/iFt2DF.8bf',
    'Ft/Ft_lib/libfftwx64_3-3.dll',
]


def fetch_ft():
    """A third plug-in family, fetched for one specific reason: these
    ship a helper DLL beside them, so they only load if the host puts the
    plug-in's own directory on the search path."""
    import urllib.parse
    import urllib.request
    for path in FT_FILES:
        out = path.rsplit('/', 1)[-1]
        if os.path.exists(out):
            continue
        url = (f'https://raw.githubusercontent.com/{FT_REPO}/HEAD/'
               + urllib.parse.quote(path))
        try:
            with urllib.request.urlopen(url, timeout=120) as r:
                open(out, 'wb').write(r.read())
        except Exception as e:
            print(f'skip: could not fetch {out}: {e}')
            return 1
    return 0


FM_REPO = 'hayabuzo/Graphic-Filters'
FM_FILES = ['01. WindyPixel/WindyPixel.8bf', '04. Zebra/Zebra.8bf']


def fetch_fm():
    """FilterMeister builds. These refuse to run at all unless the host
    implements displayPixels, so they are the guard on that."""
    import urllib.parse
    import urllib.request
    for path in FM_FILES:
        out = path.rsplit('/', 1)[-1]
        if os.path.exists(out):
            continue
        url = (f'https://raw.githubusercontent.com/{FM_REPO}/HEAD/'
               + urllib.parse.quote(path))
        try:
            with urllib.request.urlopen(url, timeout=120) as r:
                open(out, 'wb').write(r.read())
        except Exception as e:
            print(f'skip: could not fetch {out}: {e}')
            return 1
    return 0


SDK_REPO = 'templeblock/ArchiveGit'
SDK_DIR = 'Clients/MGX/PP/PluginTest/Plugins'
SDK_FILES = ['DissolveSans.8bf', 'ColorMunger.8bf', 'Propetizer.8bf']


def fetch_sdk():
    """Adobe's own SDK samples. Dissolve is the canonical filter, and
    ColorMunger is a colour-space conversion tester — which makes it the
    thing that exercises colorServices."""
    import urllib.parse
    import urllib.request
    for name in SDK_FILES:
        if os.path.exists(name):
            continue
        url = (f'https://raw.githubusercontent.com/{SDK_REPO}/HEAD/'
               + urllib.parse.quote(f'{SDK_DIR}/{name}'))
        try:
            with urllib.request.urlopen(url, timeout=120) as r:
                open(name, 'wb').write(r.read())
        except Exception as e:
            print(f'skip: could not fetch {name}: {e}')
            return 1
    return 0


def check_dissolved(out, label):
    """A dissolve replaces a random scatter of pixels, so the test is
    that roughly half changed — not none, and not all."""
    try:
        before, after = pixels('in.ppm'), pixels(out)
    except FileNotFoundError:
        print(f'FAIL ({label}): no output written')
        return 1
    if len(before) != len(after):
        print(f'FAIL ({label}): output is the wrong size')
        return 1
    same = sum(1 for a, b in zip(before, after) if a == b)
    fraction = same / len(before)
    if not 0.2 < fraction < 0.9:
        print(f'FAIL ({label}): {fraction:.0%} of bytes unchanged, expected a dissolve')
        return 1
    print(f'ok ({label}): dissolved, {fraction:.0%} of bytes untouched')
    return 0


def square():
    """64x64, because a Fourier transform wants power-of-two sides."""
    w = h = 64
    data = bytearray()
    for y in range(h):
        for x in range(w):
            data += bytes([(x * 4) % 256, (y * 4) % 256, ((x * y) // 4) % 256])
    open('square.ppm', 'wb').write(b'P6\n%d %d\n255\n' % (w, h) + bytes(data))


def check_changed(out, label):
    """The transform's maths is its own business; what the host has to
    get right is a full-size buffer that is not the input."""
    try:
        before, after = pixels('square.ppm'), pixels(out)
    except FileNotFoundError:
        print(f'FAIL ({label}): no output written')
        return 1
    if len(after) != 64 * 64 * 3:
        print(f'FAIL ({label}): output is {len(after)} bytes')
        return 1
    if after == before:
        print(f'FAIL ({label}): nothing changed')
        return 1
    colours = len({tuple(after[i:i + 3]) for i in range(0, len(after), 3)})
    print(f'ok ({label}): transformed, {colours} distinct colours out')
    return 0


def gradient():
    """A gradient, so a wrong stride reads as garbage rather than as a
    plausible flat colour."""
    w, h = 64, 48
    data = bytearray()
    for y in range(h):
        for x in range(w):
            data += bytes([x * 4 % 256, y * 5 % 256, (x + y) * 3 % 256])
    open('in.ppm', 'wb').write(b'P6\n%d %d\n255\n' % (w, h) + bytes(data))


def halves():
    """Two solid halves. A filter that mangles the row stride cannot
    leave this looking like two solid halves split down the middle."""
    w, h = 64, 48
    data = bytearray()
    for _ in range(h):
        for x in range(w):
            data += bytes([0, 0, 255] if x < w // 2 else [255, 255, 0])
    open('halves.ppm', 'wb').write(b'P6\n%d %d\n255\n' % (w, h) + bytes(data))


def check_halves(out, label):
    """The filter's exact maths is its own business; what the host has to
    get right is that each half comes back solid, the split stays at the
    midpoint, and something actually changed."""
    w, h = 64, 48
    try:
        before, after = pixels('halves.ppm'), pixels(out)
    except FileNotFoundError:
        print(f'FAIL ({label}): no output written')
        return 1
    if len(after) != w * h * 3:
        print(f'FAIL ({label}): output is {len(after)} bytes, expected {w * h * 3}')
        return 1

    def px(buf, x, y):
        i = (y * w + x) * 3
        return tuple(buf[i:i + 3])

    left = {px(after, x, y) for y in range(h) for x in range(0, w // 2)}
    right = {px(after, x, y) for y in range(h) for x in range(w // 2, w)}
    problems = []
    if len(left) != 1:
        problems.append(f'left half is not solid ({len(left)} colours)')
    if len(right) != 1:
        problems.append(f'right half is not solid ({len(right)} colours)')
    if left == right:
        problems.append('both halves came back the same colour')
    if after == before:
        problems.append('nothing changed')
    if problems:
        print(f'FAIL ({label}): ' + '; '.join(problems))
        return 1
    print(f'ok ({label}): {left.pop()} | {right.pop()}, split intact')
    return 0


def pixels(path):
    raw = open(path, 'rb').read()
    return raw[raw.index(b'255\n') + 4:]


def check(out, label):
    """`255-r` typed into all three fields means every output channel
    should be 255 minus the input's *red*."""
    try:
        before, after = pixels('in.ppm'), pixels(out)
    except FileNotFoundError:
        print(f'FAIL ({label}): no output written')
        return 1
    ok = len(before) == len(after) and all(
        255 - before[i // 3 * 3] == after[i] for i in range(len(before))
    )
    print(f'ok ({label}): every channel is 255 - input red' if ok
          else f'FAIL ({label}): pixels are wrong')
    return 0 if ok else 1


if __name__ == '__main__':
    cmd = sys.argv[1]
    if cmd == 'extract':
        extract()
    elif cmd == 'gradient':
        gradient()
    elif cmd == 'halves':
        halves()
    elif cmd == 'fetch-ft':
        sys.exit(fetch_ft())
    elif cmd == 'fetch-fm':
        sys.exit(fetch_fm())
    elif cmd == 'fetch-sdk':
        sys.exit(fetch_sdk())
    elif cmd == 'check-dissolved':
        sys.exit(check_dissolved(sys.argv[2], sys.argv[3]))
    elif cmd == 'square':
        square()
    elif cmd == 'check-changed':
        sys.exit(check_changed(sys.argv[2], sys.argv[3]))
    elif cmd == 'check':
        sys.exit(check(sys.argv[2], sys.argv[3]))
    elif cmd == 'check-halves':
        sys.exit(check_halves(sys.argv[2], sys.argv[3]))
    else:
        sys.exit(f'unknown command {cmd}')
