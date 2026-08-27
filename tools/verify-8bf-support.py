"""Helpers for tools/verify-8bf.sh: unpacking, fixtures, pixel checks.

Kept out of the shell script because the checks need real parsing, and
because a heredoc inside a heredoc is nobody's friend.
"""

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


def gradient():
    """A gradient, so a wrong stride reads as garbage rather than as a
    plausible flat colour."""
    w, h = 64, 48
    data = bytearray()
    for y in range(h):
        for x in range(w):
            data += bytes([x * 4 % 256, y * 5 % 256, (x + y) * 3 % 256])
    open('in.ppm', 'wb').write(b'P6\n%d %d\n255\n' % (w, h) + bytes(data))


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
    elif cmd == 'check':
        sys.exit(check(sys.argv[2], sys.argv[3]))
    else:
        sys.exit(f'unknown command {cmd}')
