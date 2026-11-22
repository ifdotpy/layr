trials: v2-hot 3, v2-cold 3, v3 3
| step | v2-hot | v2-cold | v3 | v2-hot + git (today) |
|---|---:|---:|---:|---:|
| first write (ms) | 13,853 | 11,072 | 11,619 | 25,472 |
| save of 10 files (ms, median of 10) | 12 | 12 | 14 | 26 |
| diff first to last state (ms) | 16 | 14 | 17 |  |
| read all files, loose (ms) | 2,098 | 3,500 | 3,144 |  |
| checkout, loose (ms) | 9,425 | 6,822 | 4,092 |  |
| pack (ms) | 62,230 | 48,927 | 50,402 | 112,632 |
| read all files, packed (ms) | 2,505 | 2,499 | 2,507 |  |
| checkout, packed (ms) | 6,005 | 5,831 | 3,750 |  |
| store before pack, allocated (MB) | 1,960 | 656 | 794 | 2,753 |
| store after pack, allocated (MB) | 192 | 192 | 306 | 498 |
