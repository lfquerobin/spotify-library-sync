# Validação USFA

16/11/2026 — Windows x64, Rust 1.98.0/MSVC.

- `cargo fmt -- --check`: aprovado.
- `cargo clippy --locked --all-targets -- -D warnings`: aprovado.
- `cargo test --locked --all-targets`: 113 testes aprovados.

Novos cenários cobrem USFA offline no CLI e menu, data de lançamento padrão,
rejeição de data de adição, períodos de mês/ano e sem limites, compilações por
tipo e grupo, colaboração em `appears_on`, crédito de artista seguido por ID,
exclusão de curtidas, artista que deixou de ser seguido, deduplicação por ISRC,
posições por contexto, troca de contexto, discografia ausente/incompleta,
criação pelo executor compartilhado e desserialização de opções de jobs antigos.

Referência de parâmetros: `lfquerobin/spotifast`,
`docs/_reference/playlist-generation.md`, seção USFA, e
`src/playlist_generation/releases.rs`.

A criação real na conta Spotify não foi executada. Os testes usam SQLite
temporário, API simulada e servidor HTTP local. Na execução restrita, os testes
HTTP não conseguem acessar o servidor; a suíte completa passou ao permitir esse
acesso. A seleção usa o catálogo local, sem implementar a busca ao vivo ou os
caches de descoberta do Spotifast. Cobertura de artistas, mercado e grupos e
atualidade de curtidas dependem das sincronizações feitas separadamente.
