# llama2.c source

`run.c` is based on upstream commit
[350e04fe35433e6d2941dce5a1f53308f87058eb](https://github.com/karpathy/llama2.c/commit/350e04fe35433e6d2941dce5a1f53308f87058eb).
The upstream file has SHA-256
`9c4f2d5c6ae01b71726d1cc37530d71e60bff0ec7cc012565f16a43c1ca658bd`.
The included MIT-licensed variant validates checkpoint dimensions and lengths,
bounds tokenizer reads and matrix offsets, and exposes the standalone
generation command. Its SHA-256 is
`2e090e3b853dda20b2806cdf1f7070c9f19b533e68138c1f5024ab92c98d8af3`.

Visor generates its synthetic checkpoint and tokenizer locally for its health
probe. Model weights and tokenizers for other uses have their own licenses.
