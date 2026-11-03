# v4: Duolingo lessons applied. 1–2 base shapes (rounded box + triangle flaps), pill eyes that stretch to act,
# detached paws, exaggerated ears and whiskers, one creased ear as the quirk.
BOX='#C9A97C'; BOX_LID='#E0C8A4'; BOX_SIDE='#A4875F'; BOX_IN='#5A4A33'; TAPE='#B69368'; INK='#5E4A2E'
CAT='#2A2D35'; CAT_DK='#15161B'; AMBER='#F2C572'

def pill(cx, cy, w, h, fill):
    return f'<rect x="{cx-w/2}" y="{cy-h/2}" width="{w}" height="{h}" rx="{min(w,h)/2}" fill="{fill}"/>'

def hako(state, size=96):
    shadow = f'<ellipse cx="52" cy="95" rx="34" ry="3" fill="#000" opacity="0.4"/>'
    front = f'<rect x="16" y="42" width="60" height="46" rx="4" fill="{BOX}"/>'
    side = f'<path d="M76 42 L86 33 L86 79 L76 88 Z" fill="{BOX_SIDE}"/>'
    top = f'<path d="M16 42 L26 33 L86 33 L76 42 Z" fill="{BOX_IN}"/>'
    tape = f'<rect x="43.5" y="42" width="5" height="46" fill="{TAPE}" opacity="0.9"/>'
    edge = f'<rect x="16" y="42" width="60" height="3" fill="{BOX_LID}"/>'
    # ears: bigger triangles; the left one has a creased tip (the quirk)
    if state == 'asleep':
        ears = f'<path d="M16 42 L26 33 L56 33 L46 42 Z" fill="{BOX_LID}"/><path d="M46 42 L56 33 L86 33 L76 42 Z" fill="{BOX}"/>'
    elif state == 'stopped':
        ears = (f'<g transform="rotate(-62 24 38)"><path d="M12 38 L36 38 L32 16 Q31 12 27 12 L21 12 Q17 12 16 16 Z" fill="{BOX_LID}"/></g>'
                f'<g transform="rotate(62 70 38)"><path d="M58 38 L82 38 L78 16 Q77 12 73 12 L67 12 Q63 12 62 16 Z" fill="{BOX_LID}"/></g>')
    elif state == 'pleased':
        ears = (f'<g transform="rotate(-26 24 38)"><path d="M12 38 L36 38 L32 13 Q31 9 27 9 L21 9 Q17 9 16 13 Z" fill="{BOX_LID}"/><path d="M18 36 L30 36 L27 17 Z" fill="{BOX_SIDE}" opacity="0.35"/></g>'
                f'<g transform="rotate(26 70 38)"><path d="M58 38 L82 38 L78 13 Q77 9 73 9 L67 9 Q63 9 62 13 Z" fill="{BOX_LID}"/><path d="M64 36 L76 36 L73 17 Z" fill="{BOX_SIDE}" opacity="0.35"/></g>')
    else:
        ears = (f'<g transform="rotate(-12 24 38)"><path d="M12 38 L36 38 L32 16 Q31 12 27 12 L21 12 Q17 12 16 16 Z" fill="{BOX_LID}"/><path d="M18 36 L30 36 L27 17 Z" fill="{BOX_SIDE}" opacity="0.35"/>'
                f'<path d="M16 16 L28 13 L22 22 Z" fill="{BOX_SIDE}" opacity="0.6"/></g>'
                f'<g transform="rotate(12 70 38)"><path d="M58 38 L82 38 L78 16 Q77 12 73 12 L67 12 Q63 12 62 16 Z" fill="{BOX_LID}"/><path d="M64 36 L76 36 L73 17 Z" fill="{BOX_SIDE}" opacity="0.35"/></g>')
    # eyes: pills. Half-closed = flat pill, open = tall pill, wide = taller & wider
    ey = 60
    if state in ('idle','reading','bat'):
        dx = 2 if state == 'reading' else 0
        eyes = pill(32+dx, ey, 13, 15, AMBER) + pill(60+dx, ey, 13, 15, AMBER) + pill(33.5+dx, ey+2, 5, 7, CAT_DK) + pill(61.5+dx, ey+2, 5, 7, CAT_DK)
    elif state == 'thinking':
        eyes = pill(32, ey-3, 11, 17, AMBER) + pill(60, ey-3, 11, 17, AMBER) + pill(32, ey-7, 4, 7, CAT_DK) + pill(60, ey-7, 4, 7, CAT_DK)
    elif state == 'asking':
        eyes = pill(32, ey+1, 15, 19, AMBER) + pill(60, ey-2, 15, 19, AMBER) + pill(32, ey+1, 7, 9, CAT_DK) + pill(60, ey-2, 7, 9, CAT_DK) + f'<circle cx="34.5" cy="{ey-2.5}" r="2" fill="#fff" opacity="0.9"/><circle cx="62.5" cy="{ey-5.5}" r="2" fill="#fff" opacity="0.9"/>'
    elif state == 'stopped':
        eyes = pill(32, ey, 18, 22, AMBER) + pill(60, ey, 18, 22, AMBER) + pill(32, ey, 9, 11, CAT_DK) + pill(60, ey, 9, 11, CAT_DK) + f'<circle cx="35" cy="{ey-4}" r="2.2" fill="#fff" opacity="0.9"/><circle cx="63" cy="{ey-4}" r="2.2" fill="#fff" opacity="0.9"/>'
    elif state == 'pleased':
        eyes = f'<path d="M24 {ey+2} q8 -10 16 0" stroke="{AMBER}" stroke-width="5" fill="none" stroke-linecap="round"/><path d="M52 {ey+2} q8 -10 16 0" stroke="{AMBER}" stroke-width="5" fill="none" stroke-linecap="round"/>'
    else:
        eyes = pill(32, ey, 14, 3.5, INK) + pill(60, ey, 14, 3.5, INK)
    nose = f'<path d="M43 72 L49 72 L46 76.5 Z" fill="{INK}"/>'
    if state == 'stopped':
        whisk = f'<path d="M28 75 L12 78 M64 75 L80 78" stroke="{INK}" stroke-width="2" stroke-linecap="round"/>'
    else:
        whisk = f'<path d="M28 73 L10 69 M28 77 L11 82 M64 73 L82 69 M64 77 L81 82" stroke="{INK}" stroke-width="2" stroke-linecap="round"/>'
    # detached paws (Duo’s detached feet), and the tail
    paws = '' if state == 'asleep' else f'<ellipse cx="32" cy="92" rx="8" ry="3.5" fill="{CAT}"/><ellipse cx="60" cy="92" rx="8" ry="3.5" fill="{CAT}"/>'
    tail = f'<path d="M86 72 q13 -4 11 -17 q-1 -7 -7 -8" fill="none" stroke="{CAT}" stroke-width="7" stroke-linecap="round"/>' if state != 'asleep' else f'<path d="M80 86 q15 2 15 -11" fill="none" stroke="{CAT}" stroke-width="7" stroke-linecap="round"/>'
    paw = ''
    if state == 'bat':
        paw = (f'<path d="M18 68 q-11 -2 -17 -13" stroke="{CAT}" stroke-width="8" stroke-linecap="round" fill="none"/><circle cx="2" cy="54" r="6.5" fill="{CAT}"/>'
               f'<rect x="-22" y="30" width="18" height="9" rx="2" fill="none" stroke="#4F6FA8" stroke-width="1.5" stroke-dasharray="3 2" transform="rotate(-24 -13 34)"/>')
    tilt = 'rotate(-7 46 66)' if state == 'asking' else ''
    return f'<svg aria-hidden="true" width="{size}" height="{size}" viewBox="0 0 100 100" style="flex: none; overflow: visible"><g transform="{tilt}">{shadow}{tail}{side}{top}{ears}{front}{tape}{edge}{paws}{paw}{eyes}{nose}{whisk}</g></svg>'

def badge(size=14):
    return (f'<svg aria-hidden="true" width="{size}" height="{size}" viewBox="6 8 88 88" style="flex: none">'
            f'<g transform="rotate(-12 24 38)"><path d="M12 38 L36 38 L32 16 Q31 12 27 12 L21 12 Q17 12 16 16 Z" fill="{BOX_LID}"/></g><g transform="rotate(12 70 38)"><path d="M58 38 L82 38 L78 16 Q77 12 73 12 L67 12 Q63 12 62 16 Z" fill="{BOX_LID}"/></g>'
            f'<rect x="16" y="42" width="60" height="46" rx="5" fill="{BOX}"/>'
            + pill(32, 62, 16, 18, AMBER) + pill(60, 62, 16, 18, AMBER) + pill(33, 64, 6, 8, CAT_DK) + pill(61, 64, 6, 8, CAT_DK) +
            f'<ellipse cx="32" cy="92" rx="9" ry="4" fill="{CAT}"/><ellipse cx="60" cy="92" rx="9" ry="4" fill="{CAT}"/>'
            f'<path d="M86 72 q13 -4 11 -17" fill="none" stroke="{CAT}" stroke-width="9" stroke-linecap="round"/></svg>')
