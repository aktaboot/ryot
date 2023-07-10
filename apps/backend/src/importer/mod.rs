use std::sync::Arc;

use apalis::{prelude::Storage, sqlite::SqliteStorage};
use async_graphql::{Context, Enum, InputObject, Object, Result, SimpleObject};
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use sea_orm::{
    prelude::DateTimeUtc, ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection,
    EntityTrait, FromJsonQueryResult, QueryFilter,
};
use serde::{Deserialize, Serialize};

use crate::{
    background::ImportMedia,
    entities::{media_import_report, prelude::MediaImportReport},
    migrator::{MediaImportSource, MetadataLot, MetadataSource},
    miscellaneous::resolver::MiscellaneousService,
    models::media::{
        AddMediaToCollection, CreateOrUpdateCollectionInput, MediaDetails, PostReviewInput,
        ProgressUpdateInput,
    },
    utils::user_id_from_ctx,
};

mod goodreads;
mod media_tracker;

#[derive(Debug, Clone, SimpleObject)]
pub struct ImportItemReview {
    date: Option<DateTimeUtc>,
    spoiler: bool,
    text: String,
}

#[derive(Debug, Clone, SimpleObject)]
pub struct ImportItemRating {
    id: Option<String>,
    review: Option<ImportItemReview>,
    rating: Option<Decimal>,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployMediaTrackerImportInput {
    /// The base url where the resource is present at
    api_url: String,
    /// An application token generated by an admin
    api_key: String,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployGoodreadsImportInput {
    // The RSS url that can be found from the user's profile
    rss_url: String,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployImportInput {
    pub source: MediaImportSource,
    pub media_tracker: Option<DeployMediaTrackerImportInput>,
    pub goodreads: Option<DeployGoodreadsImportInput>,
}

#[derive(Debug, SimpleObject)]
pub struct ImportItemSeen {
    id: Option<String>,
    ended_on: Option<DateTimeUtc>,
    show_season_number: Option<i32>,
    show_episode_number: Option<i32>,
    podcast_episode_number: Option<i32>,
}

#[derive(Debug)]
pub enum ImportItemIdentifier {
    // the identifier in case we need to fetch details
    NeedsDetails(String),
    // details are already filled and just need to be comitted to database
    AlreadyFilled(Box<MediaDetails>),
}

#[derive(Debug)]
pub struct ImportItem {
    source_id: String,
    lot: MetadataLot,
    source: MetadataSource,
    identifier: ImportItemIdentifier,
    seen_history: Vec<ImportItemSeen>,
    reviews: Vec<ImportItemRating>,
    collections: Vec<String>,
}

/// The various steps in which media importing can fail
#[derive(Debug, Enum, PartialEq, Eq, Copy, Clone, Serialize, Deserialize)]
pub enum ImportFailStep {
    /// Failed to get details from the source itself (for eg: MediaTracker, Goodreads etc.)
    ItemDetailsFromSource,
    /// Failed to get metadata from the provider (for eg: Openlibrary, IGDB etc.)
    MediaDetailsFromProvider,
}

#[derive(
    Debug, SimpleObject, FromJsonQueryResult, Serialize, Deserialize, Eq, PartialEq, Clone,
)]
pub struct ImportFailedItem {
    lot: MetadataLot,
    step: ImportFailStep,
    identifier: String,
    error: Option<String>,
}

#[derive(Debug, SimpleObject, Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct ImportDetails {
    pub total: usize,
}

#[derive(Debug)]
pub struct ImportResult {
    collections: Vec<CreateOrUpdateCollectionInput>,
    media: Vec<ImportItem>,
    failed_items: Vec<ImportFailedItem>,
}

#[derive(
    Debug, SimpleObject, Serialize, Deserialize, FromJsonQueryResult, Eq, PartialEq, Clone,
)]
pub struct ImportResultResponse {
    pub source: MediaImportSource,
    pub import: ImportDetails,
    pub failed_items: Vec<ImportFailedItem>,
}

#[derive(Default)]
pub struct ImporterQuery;

#[Object]
impl ImporterQuery {
    /// Get all the import jobs deployed by the user
    async fn media_import_reports(
        &self,
        gql_ctx: &Context<'_>,
    ) -> Result<Vec<media_import_report::Model>> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<Arc<ImporterService>>()
            .media_import_reports(user_id)
            .await
    }
}

#[derive(Default)]
pub struct ImporterMutation;

#[Object]
impl ImporterMutation {
    /// Add job to import data from various sources.
    async fn deploy_import(
        &self,
        gql_ctx: &Context<'_>,
        input: DeployImportInput,
    ) -> Result<String> {
        let user_id = user_id_from_ctx(gql_ctx).await?;
        gql_ctx
            .data_unchecked::<Arc<ImporterService>>()
            .deploy_import(user_id, input)
            .await
    }
}

#[derive(Debug)]
pub struct ImporterService {
    db: DatabaseConnection,
    media_service: Arc<MiscellaneousService>,
    import_media: SqliteStorage<ImportMedia>,
}

impl ImporterService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: &DatabaseConnection,
        media_service: Arc<MiscellaneousService>,
        import_media: &SqliteStorage<ImportMedia>,
    ) -> Self {
        Self {
            db: db.clone(),
            media_service,
            import_media: import_media.clone(),
        }
    }

    pub async fn deploy_import(
        &self,
        user_id: i32,
        mut input: DeployImportInput,
    ) -> Result<String> {
        let mut storage = self.import_media.clone();
        if let Some(s) = input.media_tracker.as_mut() {
            s.api_url = s.api_url.trim_end_matches('/').to_owned()
        }
        let job = storage
            .push(ImportMedia {
                user_id: user_id.into(),
                input,
            })
            .await
            .unwrap();
        Ok(job.to_string())
    }

    pub async fn invalidate_import_jobs(&self) -> Result<()> {
        let all_jobs = MediaImportReport::find()
            .filter(media_import_report::Column::Success.is_null())
            .all(&self.db)
            .await?;
        for job in all_jobs {
            if Utc::now() - job.started_on > Duration::hours(24) {
                tracing::info!("Invalidating job with id = {id}", id = job.id);
                let mut job: media_import_report::ActiveModel = job.into();
                job.success = ActiveValue::Set(Some(false));
                job.save(&self.db).await?;
            }
        }
        Ok(())
    }

    pub async fn media_import_reports(
        &self,
        user_id: i32,
    ) -> Result<Vec<media_import_report::Model>> {
        self.media_service.media_import_reports(user_id).await
    }

    pub async fn import_from_source(&self, user_id: i32, input: DeployImportInput) -> Result<()> {
        let db_import_job = self
            .media_service
            .start_import_job(user_id, input.source)
            .await?;
        let mut import = match input.source {
            MediaImportSource::MediaTracker => {
                media_tracker::import(input.media_tracker.unwrap()).await?
            }
            MediaImportSource::Goodreads => goodreads::import(input.goodreads.unwrap()).await?,
        };
        for col_details in import.collections.into_iter() {
            self.media_service
                .create_or_update_collection(&user_id, col_details)
                .await?;
        }
        for (idx, item) in import.media.iter().enumerate() {
            tracing::trace!(
                "Importing media with identifier = {iden}",
                iden = item.source_id
            );
            let data = match &item.identifier {
                ImportItemIdentifier::NeedsDetails(i) => {
                    self.media_service
                        .commit_media(item.lot, item.source, i)
                        .await
                }
                ImportItemIdentifier::AlreadyFilled(a) => {
                    self.media_service.commit_media_internal(*a.clone()).await
                }
            };
            let metadata = match data {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("{e:?}");
                    import.failed_items.push(ImportFailedItem {
                        lot: item.lot,
                        step: ImportFailStep::MediaDetailsFromProvider,
                        identifier: item.source_id.to_owned(),
                        error: Some(e.message),
                    });
                    continue;
                }
            };
            for seen in item.seen_history.iter() {
                self.media_service
                    .progress_update(
                        ProgressUpdateInput {
                            identifier: seen.id.clone(),
                            metadata_id: metadata.id,
                            progress: Some(100),
                            date: seen.ended_on.map(|d| d.date_naive()),
                            show_season_number: seen.show_season_number,
                            show_episode_number: seen.show_episode_number,
                            podcast_episode_number: seen.podcast_episode_number,
                        },
                        user_id,
                    )
                    .await?;
            }
            for review in item.reviews.iter() {
                let text = review.review.clone().map(|r| r.text);
                let spoiler = review.review.clone().map(|r| r.spoiler);
                let date = review.review.clone().map(|r| r.date);
                self.media_service
                    .post_review(
                        &user_id,
                        PostReviewInput {
                            identifier: review.id.clone(),
                            rating: review.rating,
                            text,
                            spoiler,
                            date: date.flatten(),
                            visibility: None,
                            metadata_id: metadata.id,
                            review_id: None,
                            season_number: None,
                            episode_number: None,
                        },
                    )
                    .await?;
            }
            for col in item.collections.iter() {
                self.media_service
                    .create_or_update_collection(
                        &user_id,
                        CreateOrUpdateCollectionInput {
                            name: col.to_string(),
                            ..Default::default()
                        },
                    )
                    .await?;
                self.media_service
                    .add_media_to_collection(
                        &user_id,
                        AddMediaToCollection {
                            collection_name: col.to_string(),
                            media_id: metadata.id,
                        },
                    )
                    .await
                    .ok();
            }
            tracing::trace!(
                "Imported item: {idx}, lot: {lot}, history count: {hist}, reviews count: {rev}",
                idx = idx,
                lot = item.lot,
                hist = item.seen_history.len(),
                rev = item.reviews.len()
            );
        }
        self.media_service
            .deploy_recalculate_summary_job(user_id)
            .await
            .ok();
        tracing::info!(
            "Imported {total} media items from {source}",
            total = import.media.len(),
            source = db_import_job.source
        );
        let details = ImportResultResponse {
            source: db_import_job.source,
            import: ImportDetails {
                total: import.media.len() - import.failed_items.len(),
            },
            failed_items: import.failed_items,
        };
        self.media_service
            .finish_import_job(db_import_job, details)
            .await?;
        Ok(())
    }
}
