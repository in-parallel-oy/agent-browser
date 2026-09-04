# Recording sound assets

`click.wav` and `keyboard.wav` are 48 kHz mono PCM16 assets embedded by the demo recorder. Keeping them uncompressed avoids a runtime audio decoder; the completed soundtrack is encoded during the existing ffmpeg mux.

They were converted from Voice Memos exports with:

```bash
ffmpeg -ss 0.095 -t 6.615 -i keyboard.m4a -ar 48000 -ac 1 -c:a pcm_s16le keyboard.wav
ffmpeg -ss 0.575 -t 0.145 -i click.m4a -af 'afade=t=in:st=0:d=0.003,afade=t=out:st=0.105:d=0.040' -ar 48000 -ac 1 -c:a pcm_s16le click.wav
```
