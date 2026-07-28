use crate::models::session::Session;
use anyhow::Result;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgPool;
use sqlx::{PgPool as Pool, Postgres};

const WRITER_LOCK_KEY: i64 = 0x485553544C455452;

struct EmbeddedMigration {
    version: i64,
    description: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[EmbeddedMigration] = &[
    EmbeddedMigration {
        version: 20231019000000,
        description: "20231019000000_initial_schema.sql",
        sql: include_str!("../../database/migrations/20231019000000_initial_schema.sql"),
    },
    EmbeddedMigration {
        version: 20251020000000,
        description: "20251020000000_add_parsed_data_columns.sql",
        sql: include_str!("../../database/migrations/20251020000000_add_parsed_data_columns.sql"),
    },
    EmbeddedMigration {
        version: 20251030000000,
        description: "20251030000000_add_idle_tracking.sql",
        sql: include_str!("../../database/migrations/20251030000000_add_idle_tracking.sql"),
    },
    EmbeddedMigration {
        version: 20251103000000,
        description: "20251103000000_add_idle_accumulation.sql",
        sql: include_str!("../../database/migrations/20251103000000_add_idle_accumulation.sql"),
    },
    EmbeddedMigration {
        version: 20251128000000,
        description: "20251128000000_add_app_renames.sql",
        sql: include_str!("../../database/migrations/20251128000000_add_app_renames.sql"),
    },
];

pub struct Database {
    pool: Pool,
    // Holding this connection keeps the session-level advisory lock alive.
    // PostgreSQL releases the lock automatically when the connection closes.
    _writer_lock: Option<PoolConnection<Postgres>>,
    is_writer: bool,
}

impl Database {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = PgPool::connect(database_url).await?;

        let mut lock_connection = pool.acquire().await?;
        let is_writer: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(WRITER_LOCK_KEY)
            .fetch_one(&mut *lock_connection)
            .await?;

        if is_writer {
            Self::run_migrations(&pool).await?;
        } else {
            log::info!("Another process owns the tracking writer lock; running read-only");
        }

        Ok(Self {
            pool,
            _writer_lock: is_writer.then_some(lock_connection),
            is_writer,
        })
    }

    pub fn is_writer(&self) -> bool {
        self.is_writer
    }

    async fn run_migrations(pool: &Pool) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS _sqlx_migrations (
                version BIGINT NOT NULL PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
                success BOOLEAN NOT NULL,
                checksum BYTEA NOT NULL,
                execution_time BIGINT NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        for migration in MIGRATIONS {
            let already_applied: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version = $1)",
            )
            .bind(migration.version)
            .fetch_one(pool)
            .await?;

            if !already_applied {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(migration.sql.as_bytes());
                let checksum = hasher.finalize().to_vec();

                let start = std::time::Instant::now();
                match sqlx::raw_sql(migration.sql).execute(pool).await {
                    Ok(_) => {
                        let execution_time = start.elapsed().as_millis() as i64;
                        sqlx::query(
                            r#"
                            INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
                            VALUES ($1, $2, true, $3, $4)
                            "#,
                        )
                        .bind(migration.version)
                        .bind(migration.description)
                        .bind(&checksum)
                        .bind(execution_time)
                        .execute(pool)
                        .await?;

                        log::info!(
                            "Applied migration: {} ({}ms)",
                            migration.description,
                            execution_time
                        );
                    }
                    Err(e) => {
                        sqlx::query(
                            r#"
                            INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
                            VALUES ($1, $2, false, $3, 0)
                            "#,
                        )
                        .bind(migration.version)
                        .bind(migration.description)
                        .bind(&checksum)
                        .execute(pool)
                        .await
                        .ok();

                        return Err(anyhow::anyhow!(
                            "Migration failed: {}: {}",
                            migration.description,
                            e
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn get_browser_page_title_rename(&self, title: &str) -> Result<Option<String>> {
        let renamed: Option<(String,)> = sqlx::query_as(
            "SELECT browser_page_title_renamed FROM sessions WHERE browser_page_title = $1 AND browser_page_title_renamed IS NOT NULL LIMIT 1"
        )
        .bind(title)
        .fetch_optional(&self.pool)
        .await?;
        Ok(renamed.map(|(r,)| r))
    }

    pub async fn get_browser_page_title_category(&self, title: &str) -> Result<Option<String>> {
        let category: Option<(String,)> = sqlx::query_as(
            "SELECT browser_page_title_category FROM sessions WHERE browser_page_title = $1 AND browser_page_title_category IS NOT NULL LIMIT 1"
        )
        .bind(title)
        .fetch_optional(&self.pool)
        .await?;
        Ok(category.map(|(c,)| c))
    }

    pub async fn get_terminal_directory_rename(&self, dir: &str) -> Result<Option<String>> {
        let renamed: Option<(String,)> = sqlx::query_as(
            "SELECT terminal_directory_renamed FROM sessions WHERE terminal_directory = $1 AND terminal_directory_renamed IS NOT NULL LIMIT 1"
        )
        .bind(dir)
        .fetch_optional(&self.pool)
        .await?;
        Ok(renamed.map(|(r,)| r))
    }

    pub async fn get_terminal_directory_category(&self, dir: &str) -> Result<Option<String>> {
        let category: Option<(String,)> = sqlx::query_as(
            "SELECT terminal_directory_category FROM sessions WHERE terminal_directory = $1 AND terminal_directory_category IS NOT NULL LIMIT 1"
        )
        .bind(dir)
        .fetch_optional(&self.pool)
        .await?;
        Ok(category.map(|(c,)| c))
    }

    pub async fn get_editor_filename_rename(&self, filename: &str) -> Result<Option<String>> {
        let renamed: Option<(String,)> = sqlx::query_as(
            "SELECT editor_filename_renamed FROM sessions WHERE editor_filename = $1 AND editor_filename_renamed IS NOT NULL LIMIT 1"
        )
        .bind(filename)
        .fetch_optional(&self.pool)
        .await?;
        Ok(renamed.map(|(r,)| r))
    }

    pub async fn get_editor_filename_category(&self, filename: &str) -> Result<Option<String>> {
        let category: Option<(String,)> = sqlx::query_as(
            "SELECT editor_filename_category FROM sessions WHERE editor_filename = $1 AND editor_filename_category IS NOT NULL LIMIT 1"
        )
        .bind(filename)
        .fetch_optional(&self.pool)
        .await?;
        Ok(category.map(|(c,)| c))
    }

    pub async fn get_tmux_window_name_rename(&self, name: &str) -> Result<Option<String>> {
        let renamed: Option<(String,)> = sqlx::query_as(
            "SELECT tmux_window_name_renamed FROM sessions WHERE tmux_window_name = $1 AND tmux_window_name_renamed IS NOT NULL LIMIT 1"
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(renamed.map(|(r,)| r))
    }

    pub async fn get_tmux_window_name_category(&self, name: &str) -> Result<Option<String>> {
        let category: Option<(String,)> = sqlx::query_as(
            "SELECT tmux_window_name_category FROM sessions WHERE tmux_window_name = $1 AND tmux_window_name_category IS NOT NULL LIMIT 1"
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(category.map(|(c,)| c))
    }

    pub async fn apply_renames_and_categories(&self, session: &mut Session) -> Result<()> {
        if let Some(renamed_app_name) = self.get_app_rename(&session.app_name).await? {
            session.app_name = renamed_app_name;
        }

        if let Some(title) = &session.browser_page_title {
            session.browser_page_title_renamed = self.get_browser_page_title_rename(title).await?;
            session.browser_page_title_category =
                self.get_browser_page_title_category(title).await?;
        }
        if let Some(dir) = &session.terminal_directory {
            session.terminal_directory_renamed = self.get_terminal_directory_rename(dir).await?;
            session.terminal_directory_category = self.get_terminal_directory_category(dir).await?;
        }
        if let Some(filename) = &session.editor_filename {
            session.editor_filename_renamed = self.get_editor_filename_rename(filename).await?;
            session.editor_filename_category = self.get_editor_filename_category(filename).await?;
        }
        if let Some(name) = &session.tmux_window_name {
            session.tmux_window_name_renamed = self.get_tmux_window_name_rename(name).await?;
            session.tmux_window_name_category = self.get_tmux_window_name_category(name).await?;
        }
        Ok(())
    }

    pub async fn insert_session(&self, session: &Session) -> Result<i32> {
        if !self.is_writer {
            return Err(anyhow::anyhow!(
                "database is read-only; another process owns recording"
            ));
        }

        let id: (i32,) = sqlx::query_as(
            r#"
            INSERT INTO sessions (
                app_name, window_name, start_time, duration, category,
                browser_url, browser_page_title, browser_notification_count,
                browser_page_title_renamed, browser_page_title_category,
                terminal_username, terminal_hostname, terminal_directory, terminal_project_name,
                terminal_directory_renamed, terminal_directory_category,
                editor_filename, editor_filepath, editor_project_path, editor_language,
                editor_filename_renamed, editor_filename_category,
                tmux_window_name, tmux_pane_count, terminal_multiplexer,
                tmux_window_name_renamed, tmux_window_name_category,
                ide_project_name, ide_file_open, ide_workspace,
                parsed_data, parsing_success, is_afk, is_idle, idle_accumulation_secs
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8,
                $9, $10,
                $11, $12, $13, $14,
                $15, $16,
                $17, $18, $19, $20,
                $21, $22,
                $23, $24, $25,
                $26, $27,
                $28, $29, $30,
                $31, $32, $33, $34, $35
            ) RETURNING id
            "#,
        )
        .bind(&session.app_name)
        .bind(&session.window_name)
        .bind(session.start_time)
        .bind(session.duration)
        .bind(&session.category)
        // Browser
        .bind(&session.browser_url)
        .bind(&session.browser_page_title)
        .bind(session.browser_notification_count)
        .bind(&session.browser_page_title_renamed)
        .bind(&session.browser_page_title_category)
        // Terminal
        .bind(&session.terminal_username)
        .bind(&session.terminal_hostname)
        .bind(&session.terminal_directory)
        .bind(&session.terminal_project_name)
        .bind(&session.terminal_directory_renamed)
        .bind(&session.terminal_directory_category)
        // Editor
        .bind(&session.editor_filename)
        .bind(&session.editor_filepath)
        .bind(&session.editor_project_path)
        .bind(&session.editor_language)
        .bind(&session.editor_filename_renamed)
        .bind(&session.editor_filename_category)
        // Multiplexer
        .bind(&session.tmux_window_name)
        .bind(session.tmux_pane_count)
        .bind(&session.terminal_multiplexer)
        .bind(&session.tmux_window_name_renamed)
        .bind(&session.tmux_window_name_category)
        // IDE
        .bind(&session.ide_project_name)
        .bind(&session.ide_file_open)
        .bind(&session.ide_workspace)
        // Metadata
        .bind(&session.parsed_data)
        .bind(session.parsing_success)
        // AFK tracking
        .bind(session.is_afk)
        .bind(session.is_idle)
        .bind(session.idle_accumulation_secs)
        .fetch_one(&self.pool)
        .await?;
        Ok(id.0)
    }

    pub async fn update_session(&self, session: &Session) -> Result<()> {
        if !self.is_writer {
            return Err(anyhow::anyhow!(
                "database is read-only; another process owns recording"
            ));
        }

        let id = session
            .id
            .ok_or_else(|| anyhow::anyhow!("cannot update a session without an id"))?;

        sqlx::query(
            r#"
            UPDATE sessions SET
                app_name = $1, window_name = $2, duration = $3, category = $4,
                browser_url = $5, browser_page_title = $6, browser_notification_count = $7,
                browser_page_title_renamed = $8, browser_page_title_category = $9,
                terminal_username = $10, terminal_hostname = $11, terminal_directory = $12,
                terminal_project_name = $13, terminal_directory_renamed = $14,
                terminal_directory_category = $15,
                editor_filename = $16, editor_filepath = $17, editor_project_path = $18,
                editor_language = $19, editor_filename_renamed = $20,
                editor_filename_category = $21,
                tmux_window_name = $22, tmux_pane_count = $23, terminal_multiplexer = $24,
                tmux_window_name_renamed = $25, tmux_window_name_category = $26,
                ide_project_name = $27, ide_file_open = $28, ide_workspace = $29,
                parsed_data = $30, parsing_success = $31, is_afk = $32, is_idle = $33,
                idle_accumulation_secs = $34
            WHERE id = $35
            "#,
        )
        .bind(&session.app_name)
        .bind(&session.window_name)
        .bind(session.duration)
        .bind(&session.category)
        .bind(&session.browser_url)
        .bind(&session.browser_page_title)
        .bind(session.browser_notification_count)
        .bind(&session.browser_page_title_renamed)
        .bind(&session.browser_page_title_category)
        .bind(&session.terminal_username)
        .bind(&session.terminal_hostname)
        .bind(&session.terminal_directory)
        .bind(&session.terminal_project_name)
        .bind(&session.terminal_directory_renamed)
        .bind(&session.terminal_directory_category)
        .bind(&session.editor_filename)
        .bind(&session.editor_filepath)
        .bind(&session.editor_project_path)
        .bind(&session.editor_language)
        .bind(&session.editor_filename_renamed)
        .bind(&session.editor_filename_category)
        .bind(&session.tmux_window_name)
        .bind(session.tmux_pane_count)
        .bind(&session.terminal_multiplexer)
        .bind(&session.tmux_window_name_renamed)
        .bind(&session.tmux_window_name_category)
        .bind(&session.ide_project_name)
        .bind(&session.ide_file_open)
        .bind(&session.ide_workspace)
        .bind(&session.parsed_data)
        .bind(session.parsing_success)
        .bind(session.is_afk)
        .bind(session.is_idle)
        .bind(session.idle_accumulation_secs)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn persist_session(&self, session: &Session) -> Result<i32> {
        if let Some(id) = session.id {
            self.update_session(session).await?;
            Ok(id)
        } else {
            self.insert_session(session).await
        }
    }

    pub async fn get_app_rename(&self, original_app_name: &str) -> Result<Option<String>> {
        let rename: Option<(String,)> =
            sqlx::query_as("SELECT renamed_app_name FROM app_renames WHERE original_app_name = $1")
                .bind(original_app_name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(rename.map(|(r,)| r))
    }
}
