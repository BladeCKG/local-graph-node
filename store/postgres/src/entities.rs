//! Support for the management of the schemas and tables we create in
//! the database for each deployment. The Postgres schemas for each
//! deployment/subgraph are tracked in the `deployment_schemas` table.
//!
//! The functions in this module are very low-level and should only be used
//! directly by the Postgres store, and nowhere else. At the same time, all
//! manipulation of entities in the database should go through this module
//! to make it easier to handle future schema changes

// We use Diesel's dynamic table support for querying the entities and history
// tables of a subgraph. Unfortunately, this support is not good enough for
// modifying data, and we fall back to generating literal SQL queries for that.
// For the `entities` table of the subgraph of subgraphs, we do map the table
// statically and use it in some cases to bridge the gap between dynamic and
// static table support, in particular in the update operation for entities.
// Diesel deeply embeds the assumption that all schema is known at compile time;
// for example, the column for a dynamic table can not implement
// `diesel::query_source::Column` since that must carry the column name as a
// constant. As a consequence, a lot of Diesel functionality is not available
// for dynamic tables.

use diesel::pg::PgConnection;
use diesel::r2d2::{ConnectionManager, PooledConnection};
use diesel::sql_types::{Integer, Text};
use diesel::Connection as _;
use diesel::RunQueryDsl;
use maybe_owned::MaybeOwned;
use std::collections::{BTreeMap, HashMap};
use std::convert::TryInto;
use std::sync::{Arc, Mutex};

use graph::components::store::EntityType;
use graph::data::subgraph::schema::POI_OBJECT;
use graph::prelude::{
    BlockNumber, Entity, EntityCollection, EntityFilter, EntityKey, EntityOrder, EntityRange,
    EthereumBlockPointer, Logger, QueryExecutionError, StoreError, StoreEvent,
    SubgraphDeploymentId,
};

use crate::block_range::block_number;
use crate::relational::Layout;

/// The size of string prefixes that we index. This is chosen so that we
/// will index strings that people will do string comparisons like
/// `=` or `!=` on; if text longer than this is stored in a String attribute
/// it is highly unlikely that they will be used for exact string operations.
/// This also makes sure that we do not put strings into a BTree index that's
/// bigger than Postgres' limit on such strings which is about 2k
pub const STRING_PREFIX_SIZE: usize = 256;

/// Marker trait for tables that store entities
pub(crate) trait EntitySource {}

/// A cache for layout objects as constructing them takes a bit of
/// computation. The cache lives as an attribute on the Store, but is managed
/// solely from this module
pub(crate) type LayoutCache = Mutex<HashMap<SubgraphDeploymentId, Arc<Layout>>>;

pub(crate) fn make_layout_cache() -> LayoutCache {
    Mutex::new(HashMap::new())
}

/// A connection into the database to handle entities. The connection is
/// specific to one subgraph, and can only handle entities from that subgraph
/// or from the metadata subgraph. Attempts to access other subgraphs will
/// generally result in a panic.
///
/// Instances of this struct must not be cached across transactions as it
/// contains a database connection
#[derive(Constructor)]
pub struct Connection<'a> {
    pub conn: MaybeOwned<'a, PooledConnection<ConnectionManager<PgConnection>>>,
    /// The layout of the actual subgraph data; entities
    /// go into this
    data: Arc<Layout>,
    /// The subgraph that is accessible through this connection
    subgraph: SubgraphDeploymentId,
}

impl Connection<'_> {
    /// Return the layout for `key`, which must refer either to the subgraph
    /// for this connection, or the metadata subgraph.
    ///
    /// # Panics
    ///
    /// If `key` does not reference the connection's subgraph or the metadata
    /// subgraph
    fn layout_for(&self, key: &EntityKey) -> &Layout {
        if &key.subgraph_id != &self.subgraph {
            panic!(
                "A connection can only be used with one subgraph and \
                 the metadata subgraph.\nThe connection for {} is also \
                 used with {}",
                self.subgraph, key.subgraph_id
            );
        }
        self.data.as_ref()
    }

    pub(crate) fn find(
        &self,
        key: &EntityKey,
        block: BlockNumber,
    ) -> Result<Option<Entity>, StoreError> {
        assert_eq!(&self.subgraph, &key.subgraph_id);
        self.data
            .find(&self.conn, &key.entity_type, &key.entity_id, block)
    }

    /// Returns a sequence of `(type, entity)`.
    /// If the entity isn't present that means it wasn't found.
    pub(crate) fn find_many(
        &self,
        ids_for_type: BTreeMap<&EntityType, Vec<&str>>,
        block: BlockNumber,
    ) -> Result<BTreeMap<EntityType, Vec<Entity>>, StoreError> {
        self.data.find_many(&self.conn, ids_for_type, block)
    }

    pub(crate) fn query<T: crate::relational_queries::FromEntityData>(
        &self,
        logger: &Logger,
        collection: EntityCollection,
        filter: Option<EntityFilter>,
        order: EntityOrder,
        range: EntityRange,
        block: BlockNumber,
        query_id: Option<String>,
    ) -> Result<Vec<T>, QueryExecutionError> {
        self.data.query(
            logger, &self.conn, collection, filter, order, range, block, query_id,
        )
    }

    pub(crate) fn conflicting_entity(
        &self,
        entity_id: &String,
        entities: Vec<EntityType>,
    ) -> Result<Option<String>, StoreError> {
        self.data
            .conflicting_entity(&self.conn, entity_id, entities)
    }

    pub(crate) fn insert(
        &self,
        key: &EntityKey,
        entity: Entity,
        ptr: &EthereumBlockPointer,
    ) -> Result<(), StoreError> {
        let layout = self.layout_for(key);
        layout.insert(&self.conn, key, entity, block_number(ptr))
    }

    /// Overwrite an entity with a new version. The `ptr` indicates
    /// at which block the new version becomes valid if it is given. If it is
    /// `None`, the entity is treated as unversioned
    pub(crate) fn update(
        &self,
        key: &EntityKey,
        entity: Entity,
        ptr: &EthereumBlockPointer,
    ) -> Result<(), StoreError> {
        let layout = self.layout_for(key);
        layout.update(&self.conn, key, entity, block_number(ptr))
    }

    pub(crate) fn delete(
        &self,
        key: &EntityKey,
        ptr: &EthereumBlockPointer,
    ) -> Result<usize, StoreError> {
        let layout = self.layout_for(key);
        layout.delete(&self.conn, key, block_number(ptr))
    }

    pub(crate) fn revert_block(
        &self,
        block_ptr: &EthereumBlockPointer,
    ) -> Result<(StoreEvent, i32), StoreError> {
        // At 1 block per 15 seconds, the maximum i32
        // value affords just over 1020 years of blocks.
        let block = block_ptr
            .number
            .try_into()
            .expect("block numbers fit into an i32");

        // Revert the block in the subgraph itself
        let (event, count) = self.data.revert_block(&self.conn, &self.subgraph, block)?;
        // Revert the meta data changes that correspond to this subgraph.
        // Only certain meta data changes need to be reverted, most
        // importantly creation of dynamic data sources. We ensure in the
        // rest of the code that we only record history for those meta data
        // changes that might need to be reverted
        Layout::revert_metadata(&self.conn, &self.subgraph, block)?;
        Ok((event, count))
    }

    pub(crate) fn update_entity_count(&self, count: i32) -> Result<(), StoreError> {
        if count == 0 {
            return Ok(());
        }

        let count_query = self.data.count_query.as_str();

        // The big complication in this query is how to determine what the
        // new entityCount should be. We want to make sure that if the entityCount
        // is NULL or the special value `-1`, it gets recomputed. Using `-1` here
        // makes it possible to manually set the `entityCount` to that value
        // to force a recount; setting it to `NULL` is not desirable since
        // `entityCount` on the GraphQL level is not nullable, and so setting
        // `entityCount` to `NULL` could cause errors at that layer; temporarily
        // returning `-1` is more palatable. To be exact, recounts have to be
        // done here, from the subgraph writer.
        //
        // The first argument of `coalesce` will be `NULL` if the entity count
        // is `NULL` or `-1`, forcing `coalesce` to evaluate its second
        // argument, the query to count entities. In all other cases,
        // `coalesce` does not evaluate its second argument
        let query = format!(
            "
            update subgraphs.subgraph_deployment
               set entity_count =
                     coalesce((nullif(entity_count, -1)) + $1,
                              ({count_query}))
             where id = $2
            ",
            count_query = count_query
        );
        let conn: &PgConnection = &self.conn;
        Ok(diesel::sql_query(query)
            .bind::<Integer, _>(count)
            .bind::<Text, _>(self.subgraph.as_str())
            .execute(conn)
            .map(|_| ())?)
    }

    pub(crate) fn transaction<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce() -> Result<T, E>,
        E: From<diesel::result::Error>,
    {
        self.conn.transaction(f)
    }

    pub(crate) fn supports_proof_of_indexing(&self) -> bool {
        self.data.tables.contains_key(&*POI_OBJECT)
    }
}
