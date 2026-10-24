# README GIFs

Each GIF shows real commands and their real output. `rec.sh` runs the steps of one scenario in a
fresh btrfs file system as root on Linux; `cast.py` turns its output into an asciicast with typing;
`agg` renders the GIF.

```sh
LAYR_BIN_DIR=$PWD/target/release bash tools/gif/rec.sh tools/gif/undo.steps > undo.out
python3 tools/gif/cast.py undo.out undo.cast "layr undo"
agg --theme github-dark --font-size 15 --line-height 1.4 undo.cast docs/assets/undo.gif
```
