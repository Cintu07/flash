# agent-latency-anatomy

Model calls are replayed from fixtures, so **decode time is not in these numbers**. What is in them is everything the runtime controls: scheduling, packing, applying, verifying, and what it manages not to do at all.

## Cold, warm, hot

| ablation | regime | p50 wall | p90 wall | computed | hit | first change | green |
| --- | --- | ---: | ---: | ---: | ---: | ---: | :-: |
| ops only | cold | 20 ms | 29 ms | 18 | 0 | 1 ms | yes |
| ops only | warm | 16 ms | 50 ms | 18 | 0 | 1 ms | yes |
| ops only | hot | 12 ms | 19 ms | 18 | 0 | 1 ms | yes |
| ops + ladder | cold | 16 ms | 19 ms | 21 | 0 | 1 ms | yes |
| ops + ladder | warm | 15 ms | 18 ms | 21 | 0 | 1 ms | yes |
| ops + ladder | hot | 14 ms | 17 ms | 21 | 0 | 1 ms | yes |
| ops + ladder + impact | cold | 14 ms | 20 ms | 21 | 0 | 1 ms | yes |
| ops + ladder + impact | warm | 13 ms | 19 ms | 21 | 0 | 1 ms | yes |
| ops + ladder + impact | hot | 14 ms | 18 ms | 21 | 0 | 1 ms | yes |
| + incremental | cold | 15 ms | 17 ms | 21 | 0 | 1 ms | yes |
| + incremental | warm | 17 ms | 22 ms | 19 | 2 | 2 ms | yes |
| + incremental | hot | 4 ms | 4 ms | 0 | 21 | 0 ms | yes |

## Where the time goes

| ablation | regime | decode | apply | verify | render | queued |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| ops only | cold | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| ops only | warm | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| ops only | hot | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| ops + ladder | cold | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| ops + ladder | warm | 0 ms | 3 ms | 0 ms | 0 ms | 0 ms |
| ops + ladder | hot | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| ops + ladder + impact | cold | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| ops + ladder + impact | warm | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| ops + ladder + impact | hot | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| + incremental | cold | 0 ms | 2 ms | 0 ms | 0 ms | 0 ms |
| + incremental | warm | 0 ms | 4 ms | 0 ms | 0 ms | 0 ms |
| + incremental | hot | 0 ms | 0 ms | 0 ms | 0 ms | 0 ms |

## Per task

| task | regime | wall | computed / nodes | saved | op resolve | fallback |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| code-01-bounds-check | cold | 17 ms | 7 / 7 | 0 ms | 100% | 0% |
| code-01-bounds-check | warm | 22 ms | 6 / 7 | 0 ms | 100% | 0% |
| code-01-bounds-check | hot | 4 ms | 0 / 7 | 6 ms | 100% | 0% |
| doc-01-quarterly-section | cold | 15 ms | 7 / 7 | 0 ms | 100% | 0% |
| doc-01-quarterly-section | warm | 11 ms | 6 / 7 | 0 ms | 100% | 0% |
| doc-01-quarterly-section | hot | 3 ms | 0 / 7 | 0 ms | 100% | 0% |
| sheets-01-totals | cold | 11 ms | 7 / 7 | 0 ms | 100% | 0% |
| sheets-01-totals | warm | 17 ms | 7 / 7 | 0 ms | 100% | 0% |
| sheets-01-totals | hot | 4 ms | 0 / 7 | 2 ms | 100% | 0% |

## Losses

Where a lever made things worse, and by how much:

- cold: + incremental is 1 ms slower than ops + ladder + impact (15 ms vs 14 ms). The extra work is real; it buys correctness rather than speed in this regime.
- warm: + incremental is 4 ms slower than ops + ladder + impact (17 ms vs 13 ms). The extra work is real; it buys correctness rather than speed in this regime.
- hot: ops + ladder is 2 ms slower than ops only (14 ms vs 12 ms). The extra work is real; it buys correctness rather than speed in this regime.
