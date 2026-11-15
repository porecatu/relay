# Contribuindo

Obrigado pelo interesse. O Porecatu Relay segue as mesmas convenções do [Porecatu](https://github.com/porecatu/terminal); o resumo operacional está no [CLAUDE.md](CLAUDE.md).

## Antes de abrir um PR

1. Leia a [arquitetura](docs/arquitetura.md) e os [ADRs](docs/adr/0000-template.md) da área.
2. Mudança no que trafega entre relay, host e cliente começa na [especificação do protocolo](https://github.com/porecatu/protocol), não aqui.
3. Mudança de decisão exige ADR novo; decisão aceita não se edita.
4. Rode:

   ```bash
   cargo fmt --all --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test
   python scripts/verify-docs.py
   ```

## Convenções

- Código e comentários de código em inglês; documentação e mensagens de commit em português do Brasil.
- [Conventional Commits](https://www.conventionalcommits.org).
- Todo arquivo de código-fonte novo começa com `// SPDX-License-Identifier: GPL-3.0-or-later`.

## Certificado de origem (DCO)

Todo commit leva a linha `Signed-off-by`, que declara que você tem o direito de contribuir o código sob a licença do projeto, nos termos do [Developer Certificate of Origin 1.1](https://developercertificate.org/):

```bash
git commit -s -m "feat: ..."
```

## Licença

Ao contribuir, você concorda que sua contribuição é licenciada sob [GPL-3.0-or-later](LICENSE).
