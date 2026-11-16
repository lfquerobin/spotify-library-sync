//! Playlist candidates are selected exclusively from SQLite. No remote library reads.
use crate::{
    database::{self, now},
    spotify::{Api, Spotify},
    sync,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Lsua,
    Ussa,
    Usfa,
}
impl Kind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Lsua => "LSUA",
            Self::Ussa => "USSA",
            Self::Usfa => "USFA",
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DateBasis {
    #[default]
    Added,
    Release,
}
#[derive(Clone, Debug, Serialize, Deserialize, Args)]
pub struct Options {
    #[arg(long, value_enum)]
    pub kind: Kind,
    /// AAAA, MM/AAAA, DD/MM/AAAA; também aceita datas ISO.
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long)]
    pub to: Option<String>,
    #[arg(
        long,
        value_enum,
        default_value = "added",
        default_value_if("kind", "usfa", "release")
    )]
    pub date_basis: DateBasis,
    /// Incluir compilações na USFA (desativado por padrão).
    #[arg(long)]
    #[serde(default)]
    pub include_compilations: bool,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub public: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub id: String,
    pub uri: String,
    pub name: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    pub name: String,
    pub public: bool,
    pub count: usize,
    pub warnings: Vec<String>,
    pub incomplete: bool,
    pub candidates: Vec<Candidate>,
}
fn date_interval(value: &str) -> Result<(String, String)> {
    let iso = if value.contains('/') {
        let parts: Vec<_> = value.split('/').collect();
        match parts.as_slice() {
            [month, year] if month.len() == 2 && year.len() == 4 => format!("{year}-{month}"),
            [day, month, year] if day.len() == 2 && month.len() == 2 && year.len() == 4 => {
                format!("{year}-{month}-{day}")
            }
            _ => bail!("Use AAAA, MM/AAAA ou DD/MM/AAAA"),
        }
    } else {
        value.to_owned()
    };
    let full = match iso.len() {
        4 => format!("{iso}-01-01"),
        7 => format!("{iso}-01"),
        10 => iso.clone(),
        _ => bail!("Data inválida"),
    };
    if !iso.bytes().enumerate().all(|(i, c)| {
        if i == 4 || i == 7 {
            c == b'-'
        } else {
            c.is_ascii_digit()
        }
    }) {
        bail!("Data inválida");
    }
    let date = NaiveDate::parse_from_str(&full, "%Y-%m-%d")?;
    if !(1..=9998).contains(&date.year()) {
        bail!("Ano deve estar entre 0001 e 9998");
    }
    let next = match iso.len() {
        4 => NaiveDate::from_ymd_opt(date.year() + 1, 1, 1).unwrap(),
        7 if date.month() == 12 => NaiveDate::from_ymd_opt(date.year() + 1, 1, 1).unwrap(),
        7 => NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1).unwrap(),
        _ => date.succ_opt().context("Data inválida")?,
    };
    Ok((date.to_string(), next.pred_opt().unwrap().to_string()))
}
impl Options {
    fn bounds(&self) -> Result<(Option<String>, Option<String>)> {
        if self.kind == Kind::Usfa && self.date_basis != DateBasis::Release {
            bail!("USFA usa a data de lançamento; informe --date-basis release");
        }
        if self.kind != Kind::Usfa && self.include_compilations {
            bail!("--include-compilations está disponível somente para USFA");
        }
        let low = self
            .from
            .as_deref()
            .map(date_interval)
            .transpose()?
            .map(|v| v.0);
        let high = self
            .to
            .as_deref()
            .or(self.from.as_deref())
            .map(date_interval)
            .transpose()?
            .map(|v| v.1);
        if low.as_ref().zip(high.as_ref()).is_some_and(|(l, h)| l > h) {
            bail!("Data inicial posterior à final");
        }
        Ok((low, high))
    }
    pub fn default_name(&self) -> Result<String> {
        self.bounds()?;
        let label = match (&self.from, &self.to) {
            (None, None) => "All".to_owned(),
            (Some(a), None) => a.replace('/', "-"),
            (None, Some(b)) => format!("Through {}", b.replace('/', "-")),
            (Some(a), Some(b)) if a == b => a.replace('/', "-"),
            (Some(a), Some(b)) => format!("{} to {}", a.replace('/', "-"), b.replace('/', "-")),
        };
        Ok(format!("{} - {label}", self.kind.prefix()))
    }
}
pub async fn preview(db: &mut SqliteConnection, options: &Options) -> Result<Preview> {
    let (low, high) = options.bounds()?;
    let name = options.name.clone().unwrap_or(options.default_name()?);
    if name.trim().is_empty() || name.chars().count() > 100 {
        bail!("Nome deve ter de 1 a 100 caracteres");
    }
    let mut tx = db.begin().await?;
    // LSUA follows the liked track's own album; unrelated discography links do not change it.
    let sql=match options.kind {
        Kind::Lsua=>"SELECT m.id,m.name,m.spotify_uri,m.is_local,m.is_playable,m.saved_at,a.release_date FROM musica m JOIN album a ON a.id=m.album_id WHERE m.is_saved=1 AND a.is_saved=0 ORDER BY m.saved_at DESC,m.id",
        Kind::Ussa=>"SELECT m.id,m.name,m.spotify_uri,m.is_local,m.is_playable,a.saved_at,a.release_date FROM album a JOIN album_musica am ON am.album_id=a.id JOIN musica m ON m.id=am.musica_id WHERE a.is_saved=1 AND m.is_saved=0 ORDER BY a.saved_at DESC,a.id,am.disc_number,am.track_number,m.id",
        // Positions belong to the release's market context; album_musica can retain old links.
        Kind::Usfa => r#"
            SELECT m.id,m.name,m.spotify_uri,m.is_local,m.is_playable,m.isrc,m.raw_json,
                   a.saved_at,a.release_date,a.album_type,
                   EXISTS(SELECT 1 FROM artista_lancamento c
                          WHERE c.album_id=a.id AND c.catalog_context=l.catalog_context
                            AND c.album_group='compilation') AS compilation_group
            FROM artista_lancamento l
            JOIN artista_seguido f ON f.artista_id=l.artista_id AND f.is_followed=1
            JOIN album a ON a.id=l.album_id
            JOIN discografia_album_position p ON p.album_id=a.id AND p.album_context=l.album_context
            JOIN musica m ON m.id=p.musica_id
            WHERE l.catalog_context=(SELECT catalog_context FROM discografia_job
                                     WHERE catalog_context IS NOT NULL
                                     ORDER BY started_at DESC,id DESC LIMIT 1)
              AND m.is_saved=0
              AND EXISTS(SELECT 1 FROM musica_artista ma
                         JOIN artista_seguido followed ON followed.artista_id=ma.artista_id
                                                     AND followed.is_followed=1
                         WHERE ma.musica_id=m.id)
            ORDER BY substr(a.release_date||'-01-01',1,10) DESC,a.id,p.position,m.id
        "#,
    };
    let rows = sqlx::query(sql).fetch_all(&mut *tx).await?;
    let mut ids = HashSet::new();
    let mut uris = HashSet::new();
    let mut recordings = HashSet::new();
    let mut candidates = Vec::new();
    let mut excluded = 0;
    for row in rows {
        if options.kind == Kind::Usfa {
            if !options.include_compilations
                && (row.get::<Option<String>, _>("album_type").as_deref() == Some("compilation")
                    || row.get::<i64, _>("compilation_group") != 0)
            {
                continue;
            }
            // USFA needs a valid release date even for an unbounded period.
            if !row
                .get::<Option<String>, _>("release_date")
                .is_some_and(|date| date_interval(&date).is_ok())
            {
                continue;
            }
        }
        if low.is_some() || high.is_some() {
            let value: Option<String> = row.get(if options.date_basis == DateBasis::Added {
                "saved_at"
            } else {
                "release_date"
            });
            let range = value.and_then(|v| {
                if options.date_basis == DateBasis::Added {
                    chrono::DateTime::parse_from_rfc3339(&v).ok().map(|d| {
                        let s = d.with_timezone(&chrono::Utc).date_naive().to_string();
                        (s.clone(), s)
                    })
                } else {
                    date_interval(&v).ok()
                }
            });
            let Some((start, end)) = range else { continue };
            if low.as_ref().is_some_and(|l| end < *l) || high.as_ref().is_some_and(|h| start > *h) {
                continue;
            }
        }
        let id: String = row.get("id");
        let uri: Option<String> = row.get("spotify_uri");
        let Some(uri) = uri.filter(|v| v.strip_prefix("spotify:track:").is_some_and(valid_id))
        else {
            excluded += 1;
            continue;
        };
        if row.get::<Option<i64>, _>("is_local") == Some(1)
            || row.get::<Option<i64>, _>("is_playable") == Some(0)
        {
            excluded += 1;
            continue;
        }
        let recording = if options.kind == Kind::Usfa {
            if uri.strip_prefix("spotify:track:") != Some(id.as_str()) {
                excluded += 1;
                continue;
            }
            let raw: Value = serde_json::from_str(row.get("raw_json"))?;
            row.get::<Option<String>, _>("isrc")
                .filter(|isrc| !isrc.is_empty())
                .map(|isrc| format!("isrc:{isrc}"))
                .or_else(|| {
                    raw.pointer("/linked_from/uri")
                        .and_then(Value::as_str)
                        .filter(|uri| !uri.is_empty())
                        .map(|uri| format!("linked:{uri}"))
                })
                .unwrap_or_else(|| uri.clone())
        } else {
            uri.clone()
        };
        if !ids.contains(&id) && !uris.contains(&uri) && !recordings.contains(&recording) {
            ids.insert(id.clone());
            uris.insert(uri.clone());
            recordings.insert(recording);
            candidates.push(Candidate {
                id,
                uri,
                name: row.get("name"),
            });
        }
    }
    let states=sqlx::query("SELECT collection,initial_import_completed_at,last_incremental_completed_at,last_full_reconciliation_at,last_coverage FROM library_sync_state ORDER BY collection").fetch_all(&mut *tx).await?;
    let mut warnings=vec!["Seleção feita somente no SQLite; alterações posteriores no Spotify não são verificadas. Sincronize separadamente se desejar atualizar a base.".into()];
    let mut incomplete = false;
    for row in states {
        let collection: String = row.get("collection");
        let initial: Option<String> = row.get("initial_import_completed_at");
        incomplete |= initial.is_none();
        warnings.push(format!("{collection}: base inicial {}; última atualização {}; última reconciliação {}; cobertura {}",if initial.is_some(){"concluída"}else{"incompleta"},row.get::<Option<String>,_>("last_incremental_completed_at").or(initial).unwrap_or_else(||"nunca".into()),row.get::<Option<String>,_>("last_full_reconciliation_at").unwrap_or_else(||"nunca".into()),row.get::<String,_>("last_coverage")));
    }
    if options.kind == Kind::Ussa {
        let pending:i64=sqlx::query_scalar("SELECT COUNT(*) FROM album WHERE is_saved=1 AND (tracks_fetched_at IS NULL OR tracks_imported_total IS NULL OR total_tracks IS NULL OR tracks_imported_total<>total_tracks)").fetch_one(&mut *tx).await?;
        incomplete |= pending > 0;
        if pending > 0 {
            warnings.push(format!("{pending} álbuns salvos com faixas incompletas; somente as faixas já cadastradas poderão entrar."));
        }
    }
    if options.kind == Kind::Usfa {
        let latest = sqlx::query("SELECT catalog_context,started_at,completed_at,status FROM discografia_job WHERE catalog_context IS NOT NULL ORDER BY started_at DESC,id DESC LIMIT 1")
            .fetch_optional(&mut *tx).await?;
        if let Some(job) = latest {
            let catalog: String = job.get("catalog_context");
            let pending: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT l.album_id) FROM artista_lancamento l JOIN artista_seguido f ON f.artista_id=l.artista_id AND f.is_followed=1 LEFT JOIN discografia_album_queue q ON q.album_id=l.album_id AND q.album_context=l.album_context WHERE l.catalog_context=? AND (q.state IS NULL OR q.state<>'completed')")
                .bind(&catalog).fetch_one(&mut *tx).await?;
            let missing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artista_seguido f LEFT JOIN discografia_cache c ON c.artista_id=f.artista_id AND c.catalog_context=? WHERE f.is_followed=1 AND (c.last_full_at IS NULL OR c.needs_full_refresh=1)")
                .bind(&catalog).fetch_one(&mut *tx).await?;
            incomplete |=
                pending > 0 || missing > 0 || job.get::<String, _>("status") != "completed";
            warnings.push(format!("Discografias: última execução {}; conclusão {}; {missing} artistas sem catálogo completo neste contexto; {pending} lançamentos com faixas incompletas. Atualize separadamente com discography sync / discography tracks.", job.get::<String,_>("started_at"), job.get::<Option<String>,_>("completed_at").unwrap_or_else(|| "pendente".into())));
        } else {
            incomplete = true;
            warnings.push(
                "Discografias ainda não importadas; execute discography sync separadamente.".into(),
            );
        }
        warnings.push("USFA usa somente o contexto mais recente de discografias e os artistas atualmente seguidos na base local. Curtidas e artistas seguidos não são conferidos ao vivo.".into());
    }
    let running: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sync_job WHERE status<>'completed'")
            .fetch_one(&mut *tx)
            .await?;
    if running > 0 {
        incomplete = true;
        warnings.push("Há uma sincronização da biblioteca pendente.".into());
    }
    if excluded > 0 {
        warnings.push(format!("{excluded} registros sem URI válida, locais ou explicitamente indisponíveis foram excluídos."));
    }
    tx.commit().await?;
    Ok(Preview {
        count: candidates.len(),
        name,
        public: options.public,
        warnings,
        incomplete,
        candidates,
    })
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())
}
pub async fn freeze(
    db: &mut SqliteConnection,
    options: &Options,
    preview: &Preview,
) -> Result<String> {
    if preview.count == 0 {
        bail!("Nenhuma música encontrada; nenhuma playlist foi criada");
    }
    let account: String = sqlx::query_scalar("SELECT id FROM account LIMIT 1")
        .fetch_optional(&mut *db)
        .await?
        .context("Banco sem conta vinculada; execute login antes de criar uma playlist")?;
    let id = format!(
        "playlist-{}-{:016x}",
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u64>()
    );
    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO playlist_job(id,account_id,kind,name,public,options_json,total_items,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?)")
        .bind(&id).bind(account).bind(options.kind.prefix()).bind(&preview.name).bind(preview.public).bind(serde_json::to_string(options)?).bind(preview.count as i64).bind(now()).bind(now()).execute(&mut *tx).await?;
    for (i, item) in preview.candidates.iter().enumerate() {
        sqlx::query("INSERT INTO playlist_job_item VALUES(?,?,?,?)")
            .bind(&id)
            .bind(i as i64)
            .bind(&item.id)
            .bind(&item.uri)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id)
}
#[async_trait]
pub trait PlaylistApi: Api {
    async fn create_playlist(
        &mut self,
        name: &str,
        public: bool,
        description: &str,
    ) -> Result<Value>;
    async fn append_playlist(&mut self, id: &str, uris: &[String]) -> Result<Value>;
    async fn own_playlists(&mut self, offset: i64) -> Result<Value>;
    async fn playlist_items(&mut self, id: &str, offset: i64) -> Result<Value>;
}
#[async_trait]
impl PlaylistApi for Spotify {
    async fn create_playlist(
        &mut self,
        name: &str,
        public: bool,
        description: &str,
    ) -> Result<Value> {
        self.write_request(
            "/me/playlists",
            &json!({"name":name,"public":public,"description":description}),
        )
        .await
    }
    async fn append_playlist(&mut self, id: &str, uris: &[String]) -> Result<Value> {
        if !valid_id(id)
            || uris.is_empty()
            || uris.len() > 100
            || uris
                .iter()
                .any(|u| !u.strip_prefix("spotify:track:").is_some_and(valid_id))
        {
            bail!("Playlist ou lote inválido");
        }
        self.write_request(&format!("/playlists/{id}/items"), &json!({"uris":uris}))
            .await
    }
    async fn own_playlists(&mut self, offset: i64) -> Result<Value> {
        self.request(
            "/me/playlists",
            &[("limit", "50".into()), ("offset", offset.to_string())],
        )
        .await
    }
    async fn playlist_items(&mut self, id: &str, offset: i64) -> Result<Value> {
        if !valid_id(id) {
            bail!("ID de playlist inválido");
        }
        self.request(
            &format!("/playlists/{id}/items"),
            &[("limit", "50".into()), ("offset", offset.to_string())],
        )
        .await
    }
}
async fn checkpoint(db: &mut SqliteConnection, job: &str, confirmed: i64) -> Result<()> {
    sqlx::query("UPDATE playlist_job SET confirmed_count=?,inflight_count=0,state='filling',last_error=NULL,updated_at=? WHERE id=?").bind(confirmed).bind(now()).bind(job).execute(db).await?;
    Ok(())
}
pub async fn record_error(
    db: &mut SqliteConnection,
    job: &str,
    error: &anyhow::Error,
) -> Result<()> {
    sync::remember_cooldown(db, error).await?;
    sqlx::query("UPDATE playlist_job SET last_error=?,updated_at=? WHERE id=?")
        .bind(format!("{error:#}"))
        .bind(now())
        .bind(job)
        .execute(db)
        .await?;
    Ok(())
}
fn marker(job: &str) -> String {
    format!("spotify-library-sync:{job}")
}
// Reads only playlist state, never liked tracks, saved albums, artists, or their tracks.
pub async fn run<A: PlaylistApi>(
    db: &mut SqliteConnection,
    api: &mut A,
    job: &str,
) -> Result<Value> {
    sync::check_cooldown(db).await?;
    let row = sqlx::query("SELECT * FROM playlist_job WHERE id=?")
        .bind(job)
        .fetch_one(&mut *db)
        .await?;
    if row.get::<String, _>("state") == "completed" {
        return status(db).await;
    }
    let profile = api.get("/me", None).await?;
    let account = profile["id"].as_str().context("Perfil sem ID")?;
    if account != row.get::<String, _>("account_id") {
        bail!("Esta playlist pertence a outra conta Spotify");
    }
    database::bind_account(db, account).await?;
    let desired = sqlx::query_scalar::<_, String>(
        "SELECT uri FROM playlist_job_item WHERE job_id=? ORDER BY position",
    )
    .bind(job)
    .fetch_all(&mut *db)
    .await?;
    let mut playlist: Option<String> = row.get("playlist_id");
    let state: String = row.get("state");
    if playlist.is_none() && state == "creating" {
        let mut offset = 0;
        let mut expected = None;
        let mut found = None;
        loop {
            let page: crate::spotify::Page =
                serde_json::from_value(api.own_playlists(offset).await?)?;
            let end = page.validate(offset, expected)?;
            expected = Some(page.total);
            for item in &page.items {
                if item["description"]
                    .as_str()
                    .is_some_and(|s| s.contains(&marker(job)))
                    && item.pointer("/owner/id").and_then(Value::as_str) == Some(account)
                {
                    let id = item["id"]
                        .as_str()
                        .filter(|s| valid_id(s))
                        .context("Playlist recuperada sem ID")?
                        .to_owned();
                    if found.is_some() {
                        bail!("Mais de uma playlist encontrada para este job; confira no Spotify antes de continuar");
                    }
                    found = Some(id);
                }
            }
            offset += page.items.len() as i64;
            if end {
                break;
            }
        }
        playlist=Some(found.context("Criação anterior sem confirmação. A playlist ainda não foi localizada; aguarde e retome. Nenhuma outra será criada automaticamente. Confira no Spotify antes de abandonar este job.")?);
    }
    if playlist.is_none() {
        sqlx::query("UPDATE playlist_job SET state='creating',updated_at=? WHERE id=?")
            .bind(now())
            .bind(job)
            .execute(&mut *db)
            .await?;
        let response = match api
            .create_playlist(
                row.get("name"),
                row.get::<i64, _>("public") == 1,
                &format!("Criada a partir do catálogo local. [{}]", marker(job)),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if error.is::<crate::spotify::WriteRejected>()
                    || error.is::<crate::spotify::BudgetExhausted>()
                {
                    sqlx::query("UPDATE playlist_job SET state='ready',updated_at=? WHERE id=?")
                        .bind(now())
                        .bind(job)
                        .execute(&mut *db)
                        .await?;
                }
                return Err(error);
            }
        };
        playlist = Some(
            response["id"]
                .as_str()
                .filter(|s| valid_id(s))
                .context("Criação sem ID; retome para localizar a playlist")?
                .to_owned(),
        );
    }
    let playlist = playlist.unwrap();
    sqlx::query("UPDATE playlist_job SET playlist_id=?,state='filling',updated_at=? WHERE id=?")
        .bind(&playlist)
        .bind(now())
        .bind(job)
        .execute(&mut *db)
        .await?;
    let mut confirmed = row.get::<i64, _>("confirmed_count") as usize;
    // On every resume verify the entire remote prefix. An uncertain append may already have succeeded.
    if state != "ready" {
        let mut actual = Vec::new();
        let mut expected = None;
        loop {
            let page: crate::spotify::Page =
                serde_json::from_value(api.playlist_items(&playlist, actual.len() as i64).await?)?;
            let end = page.validate(actual.len() as i64, expected)?;
            expected = Some(page.total);
            if page.total as usize > desired.len() {
                bail!("Playlist modificada no Spotify: total excede o plano local; envio pausado");
            }
            for item in page.items {
                let track = item
                    .get("item")
                    .or_else(|| item.get("track"))
                    .context("Item de playlist indisponível")?;
                // Relinking can change the playable URI; compare the original identity if supplied.
                let uri = track
                    .pointer("/linked_from/uri")
                    .or_else(|| track.get("uri"))
                    .and_then(Value::as_str)
                    .context("Faixa da playlist sem URI")?;
                let candidate = desired
                    .get(actual.len())
                    .context("Playlist maior que a seleção")?;
                if uri != candidate
                    && track.get("uri").and_then(Value::as_str) != Some(candidate.as_str())
                {
                    bail!("Playlist modificada no Spotify: conteúdo diverge da seleção congelada; envio pausado");
                }
                actual.push(uri.to_owned());
            }
            if end {
                break;
            }
        }
        let upper = confirmed + row.get::<i64, _>("inflight_count") as usize;
        if actual.len() != confirmed && actual.len() != upper {
            bail!("Playlist modificada ou lote parcialmente confirmado; envio pausado para evitar duplicação");
        }
        confirmed = actual.len();
        checkpoint(db, job, confirmed as i64).await?;
    }
    while confirmed < desired.len() {
        let end = (confirmed + 100).min(desired.len());
        sqlx::query("UPDATE playlist_job SET inflight_count=?,updated_at=? WHERE id=?")
            .bind((end - confirmed) as i64)
            .bind(now())
            .bind(job)
            .execute(&mut *db)
            .await?;
        let response = api
            .append_playlist(&playlist, &desired[confirmed..end])
            .await?;
        if response["snapshot_id"].as_str().is_none() {
            bail!("Lote sem confirmação válida; retome para conferir a playlist");
        }
        confirmed = end;
        checkpoint(db, job, confirmed as i64).await?;
        eprintln!("Playlist: {confirmed}/{} músicas enviadas", desired.len());
    }
    sqlx::query(
        "UPDATE playlist_job SET state='completed',last_error=NULL,updated_at=? WHERE id=?",
    )
    .bind(now())
    .bind(job)
    .execute(&mut *db)
    .await?;
    Ok(
        json!({"job":job,"state":"completed","tracks":confirmed,"url":format!("https://open.spotify.com/playlist/{playlist}")}),
    )
}
pub async fn status(db: &mut SqliteConnection) -> Result<Value> {
    let rows=sqlx::query("SELECT id,name,kind,public,state,playlist_id,confirmed_count,total_items,last_error,created_at FROM playlist_job ORDER BY created_at DESC,id DESC LIMIT 100").fetch_all(db).await?;
    Ok(json!(rows.iter().map(|r|json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),"kind":r.get::<String,_>("kind"),"public":r.get::<i64,_>("public")==1,"state":r.get::<String,_>("state"),"playlist_id":r.get::<Option<String>,_>("playlist_id"),"confirmed":r.get::<i64,_>("confirmed_count"),"total":r.get::<i64,_>("total_items"),"last_error":r.get::<Option<String>,_>("last_error"),"created_at":r.get::<String,_>("created_at")})).collect::<Vec<_>>()))
}
