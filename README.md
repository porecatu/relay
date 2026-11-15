# Porecatu Relay

Servidor **auto-hospedado** que liga o [Porecatu](https://github.com/porecatu/terminal) desktop ao app [Porecatu Remote](https://github.com/porecatu/remote), para acessar do celular uma sessão viva do Porecatu.

**Estado:** só documentação. Nenhum código ainda; ver o [roadmap](docs/roadmap.md).

## O que ele é

- **Cego.** Encaminha bytes cifrados de ponta a ponta entre o desktop e o celular. Não tem as chaves da sessão, não lê terminais, teclas, títulos nem diretórios.
- **Sem contas.** Quem pode registrar um desktop é quem tem um **token do operador**, definido no arquivo de configuração. A identidade do desktop é uma chave; a do celular também, e quem confere a do celular é o próprio desktop.
- **Sem persistência.** Nada vai para disco além do que o operador escolher logar — e os logs nunca têm conteúdo.
- **Auto-hospedado.** Não há relay público oficial. Você roda o seu: um VPS, um servidor de casa, um contêiner atrás do proxy reverso que você já usa.

```
Porecatu desktop  ──wss──▶  Porecatu Relay  ◀──wss──  Porecatu Remote
        └──────────── sessão Noise ponta a ponta ────────────┘
```

O protocolo que os três falam é especificado em [porecatu/protocol](https://github.com/porecatu/protocol/blob/main/docs/especificacao/00-visao-geral.md). Este repositório implementa só a parte do relay: [transporte e envelope](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md).

## Início rápido (planejado)

> Nada abaixo existe ainda. É o alvo da [etapa R3 do roadmap](docs/roadmap.md).

```bash
# 1. Gere um token para o seu desktop
porecatu-relay token new --name notebook

# 2. Rode com TLS embutido
porecatu-relay --config /etc/porecatu-relay/relay.toml

# ou em contêiner, atrás do seu proxy reverso
docker run -p 127.0.0.1:8080:8080 -v ./relay.toml:/etc/porecatu-relay/relay.toml ghcr.io/porecatu/relay
```

No Porecatu desktop, em **Configurações → Acesso remoto**, informe a URL do relay (`wss://relay.seudominio.com.br`) e o token. O pareamento do celular é por QR code, mostrado pelo desktop; o relay não participa dele.

A configuração completa, comentada, está em [docs/reference/relay.example.toml](docs/reference/relay.example.toml).

## Documentação

- [Visão de produto](docs/prd/prd-000-visao-de-produto.md) e [operação do relay](docs/prd/prd-001-operacao-do-relay.md)
- [Arquitetura](docs/arquitetura.md)
- [Decisões (ADRs)](docs/adr/0000-template.md)
- [Roadmap](docs/roadmap.md)
- [Modelo de ameaça do protocolo](https://github.com/porecatu/protocol/blob/main/docs/especificacao/modelo-de-ameaca.md)

## Projetos relacionados

| Projeto | Papel |
|---|---|
| [porecatu/terminal](https://github.com/porecatu/terminal) | O emulador de terminal; o **host** da sessão remota |
| [porecatu/protocol](https://github.com/porecatu/protocol) | Especificação do protocolo, vetores de teste e implementações de referência |
| [porecatu/remote](https://github.com/porecatu/remote) | O app mobile (Android e iOS) |

## Licença

[GPL-3.0-or-later](LICENSE).
