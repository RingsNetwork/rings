# llms.txt

The repository keeps an `llms.txt` at its root, published at
[rings.rs/llms.txt](https://rings.rs/llms.txt), following the
[llms.txt convention](https://llmstxt.org/): a plain-text map of the project for coding
agents. It says what Rings is, which document is authoritative for what, how the crates are
layered, how each target is built and tested, and which conventions a change must keep. Point
an agent at it before it builds on Rings or changes it.

The file is maintained with the code — where it and the code disagree, the code wins and the
file is corrected in the same change. This is the current file:

```text
{{#include ../../llms.txt}}
```
