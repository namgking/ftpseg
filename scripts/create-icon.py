"""Render the matching ftpseg icon with Pillow; no external renderer needed."""
from pathlib import Path
from PIL import Image, ImageDraw
import numpy as np

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'assets'
OUT.mkdir(exist_ok=True)
S = 4
N = 512 * S

def gradient(stops, start, end):
    y, x = np.mgrid[0:N, 0:N] / S
    dx, dy = end[0]-start[0], end[1]-start[1]
    t = np.clip(((x-start[0])*dx+(y-start[1])*dy)/(dx*dx+dy*dy), 0, 1)
    colors = np.array([tuple(bytes.fromhex(c)) for _, c in stops])
    result = np.stack([np.interp(t, [p for p, _ in stops], colors[:, k]) for k in range(3)], axis=-1)
    return Image.fromarray(np.uint8(result)).convert('RGBA')

def stroke(points, width, color=None, grad=None):
    mask = Image.new('L', (N, N))
    d = ImageDraw.Draw(mask)
    pts = [(round(x*S), round(y*S)) for x,y in points]
    d.line(pts, fill=255, width=round(width*S), joint='curve')
    r = width*S/2
    for x,y in pts:
        d.ellipse((round(x-r), round(y-r), round(x+r), round(y+r)), fill=255)
    layer = grad or Image.new('RGBA', (N,N), color)
    icon.paste(layer, (0,0), mask)

icon = Image.new('RGBA', (N,N))
mask = Image.new('L', (N,N)); ImageDraw.Draw(mask).rounded_rectangle((24*S,24*S,488*S,488*S), radius=108*S, fill=255)
icon.paste(gradient([(0,'244E9A'),(.52,'142B57'),(1,'0B1834')],(64,32),(448,480)),(0,0),mask)
border = Image.new('RGBA', (N,N)); ImageDraw.Draw(border).rounded_rectangle((25*S,25*S,487*S,487*S),radius=107*S,outline=(138,183,240,56),width=2*S); icon=Image.alpha_composite(icon,border)
streams=gradient([(0,'C7F4FF'),(1,'50C8F5')],(164,104),(342,342))
for sign in [-1,1]:
    pts=[(256+sign*86,119),(256+sign*86,193)]
    for t in np.linspace(0,1,25):
        # Quadratic curve from (170,193) to (194,241), mirrored.
        offset=86-24*t*t
        yy=(1-t)**2*193+2*(1-t)*t*217+t*t*241
        pts.append((256+sign*offset, yy))
    pts.append((256+sign*34,269))
    stroke(pts,24,grad=streams)
stroke([(256,112),(256,295)],32,color='#F1FAFF')
stroke([(207,279),(256,329),(305,279)],32,color='#F1FAFF')
pts=[(139,350),(139,383)]
for t in np.linspace(0,1,18): pts.append(((1-t)**2*139+2*(1-t)*t*139+t*t*157,(1-t)**2*383+2*(1-t)*t*401+t*t*401))
pts.append((355,401))
for t in np.linspace(0,1,18): pts.append(((1-t)**2*355+2*(1-t)*t*373+t*t*373,(1-t)**2*401+2*(1-t)*t*401+t*t*383))
pts.append((373,350))
stroke(pts,26,grad=gradient([(0,'50DDDF'),(1,'4094FF')],(136,350),(376,412)))
icon=icon.resize((512,512),Image.Resampling.LANCZOS)
icon.save(OUT/'ftpseg.png')
icon.save(OUT/'ftpseg.ico',sizes=[(16,16),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)])
with Image.open(OUT/'ftpseg.ico') as check:
    expected={(n,n) for n in [16,24,32,48,64,128,256]}
    assert check.ico.sizes()==expected
print('Created SVG-matched PNG and ICO with 7 sizes')
