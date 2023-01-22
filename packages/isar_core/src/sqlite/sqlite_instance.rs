use super::sqlite3::SQLite3;
use super::sqlite_collection::{SQLiteCollection, SQLiteProperty};
use super::sqlite_insert::SQLiteInsert;
use super::sqlite_query_builder::SQLiteQueryBuilder;
use super::sqlite_schema_manager::SQLiteSchemaManager;
use super::sqlite_txn::SQLiteTxn;
use crate::common::instance::get_or_open_instance;
use crate::common::schema::{hash_schema, verify_schema};
use crate::core::error::{IsarError, Result};
use crate::core::instance::{CompactCondition, IsarInstance};
use crate::core::schema::IsarSchema;
use intmap::IntMap;
use itertools::Itertools;
use once_cell::sync::Lazy;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use thread_local::ThreadLocal;

static INSTANCES: Lazy<RwLock<IntMap<Arc<SQLiteInstance>>>> =
    Lazy::new(|| RwLock::new(IntMap::new()));

pub struct SQLiteInstance {
    path: String,
    sqlite: ThreadLocal<RefCell<Option<SQLite3>>>,
    collections: IntMap<SQLiteCollection>,
    collection_ids: Vec<u64>,
    schema_hash: u64,
}

impl SQLiteInstance {
    fn open_instance(
        name: &str,
        dir: Option<&str>,
        schema: IsarSchema,
        relaxed_durability: bool,
    ) -> Result<Self> {
        if let Some(dir) = dir {
            verify_schema(&schema)?;
            let schema_hash = hash_schema(schema.clone());

            let mut path_buf = PathBuf::from(dir);
            path_buf.push(format!("{}.sqlite", name));
            let path = path_buf.as_path().to_str().unwrap().to_string();

            let sqlite = SQLite3::open(&path).unwrap();
            let schema_manager = SQLiteSchemaManager::new(&sqlite);
            schema_manager.perform_migration(&schema)?;

            let (collections, collection_ids) = Self::get_collections(&schema);

            Ok(Self {
                path,
                sqlite: ThreadLocal::new(),
                collections: collections,
                collection_ids: collection_ids,
                schema_hash,
            })
        } else {
            Err(IsarError::IllegalArg {
                message: "Please provide a valid directory.".to_string(),
            })
        }
    }

    fn get_collections(schema: &IsarSchema) -> (IntMap<SQLiteCollection>, Vec<u64>) {
        let mut collections = IntMap::new();
        let mut collection_ids = Vec::new();
        for collection_schema in &schema.collections {
            let properties = collection_schema
                .properties
                .iter()
                .filter_map(|p| {
                    if let Some(name) = &p.name {
                        let prop = SQLiteProperty::new(name, p.data_type, p.get_target_id());
                        Some(prop)
                    } else {
                        None
                    }
                })
                .collect_vec();
            let collection = SQLiteCollection::new(collection_schema.name.clone(), properties);
            let collection_id = collection_schema.get_id();
            collections.insert(collection_id, collection);
            collection_ids.push(collection_id);
        }
        (collections, collection_ids)
    }
}

impl IsarInstance for SQLiteInstance {
    type Txn = SQLiteTxn;

    type Insert<'a> = SQLiteInsert<'a>;

    type QueryBuilder<'a> = SQLiteQueryBuilder<'a>;

    fn open(
        name: &str,
        dir: Option<&str>,
        schema: IsarSchema,
        _max_size_mib: usize,
        relaxed_durability: bool,
        _compact_condition: Option<CompactCondition>,
    ) -> Result<Arc<Self>> {
        get_or_open_instance(&INSTANCES, name, schema, move |schema| {
            Self::open_instance(name, dir, schema, relaxed_durability)
        })
    }

    fn schema_hash(&self) -> u64 {
        self.schema_hash
    }

    fn collection_id(&self, index: usize) -> Option<u64> {
        self.collection_ids.get(index).copied()
    }

    fn begin_txn(&self, write: bool) -> Result<Self::Txn> {
        let sqlite = self
            .sqlite
            .get_or_try(|| -> Result<RefCell<Option<SQLite3>>> {
                let sqlite = SQLite3::open(&self.path)?;
                Ok(RefCell::new(Some(sqlite)))
            })
            .unwrap()
            .take();
        let sqlite = if let Some(sqlite) = sqlite {
            sqlite
        } else {
            SQLite3::open(&self.path)?
        };
        SQLiteTxn::new(sqlite, write)
    }

    fn commit_txn(&self, txn: Self::Txn) -> Result<()> {
        let sqlite = txn.commit()?;
        if let Some(cell) = self.sqlite.get() {
            cell.replace(Some(sqlite));
        }
        Ok(())
    }

    fn abort_txn(&self, txn: Self::Txn) {
        if let Ok(sqlite) = txn.abort() {
            if let Some(cell) = self.sqlite.get() {
                cell.replace(Some(sqlite));
            }
        }
    }

    fn query<'a>(&'a self, collection_id: u64) -> Result<Self::QueryBuilder<'a>> {
        let collection = self
            .collections
            .get(collection_id)
            .ok_or(IsarError::IllegalArg {
                message: "Invalid collection id.".to_string(),
            })?;
        let query_builder = SQLiteQueryBuilder::new(collection, &self.collections);
        Ok(query_builder)
    }

    fn insert<'a>(
        &'a self,
        txn: &'a mut Self::Txn,
        collection_id: u64,
        count: usize,
    ) -> Result<Self::Insert<'a>> {
        let collection = self
            .collections
            .get(collection_id)
            .ok_or(IsarError::IllegalArg {
                message: "Invalid collection id.".to_string(),
            })?;
        let insert = SQLiteInsert::new(txn, collection, &self.collections, count);
        Ok(insert)
    }
}

mod test {
    use super::SQLiteInstance;
    use crate::core::data_type::DataType;
    use crate::core::filter::IsarFilterBuilder;
    use crate::core::filter::IsarValue;
    use crate::core::insert::IsarInsert;
    use crate::core::instance::IsarInstance;
    use crate::core::query::IsarCursor;
    use crate::core::query::IsarQuery;
    use crate::core::query_builder::IsarQueryBuilder;
    use crate::core::reader::IsarReader;
    use crate::core::schema::{CollectionSchema, IndexSchema, IsarSchema, PropertySchema};
    use crate::core::writer::IsarWriter;
    use crate::sqlite::sqlite_filter::*;
    use crate::sqlite::sqlite_query_builder::SQLiteQueryBuilder;

    #[test]
    fn test_exec() {
        let schema = IsarSchema::new(vec![CollectionSchema::new(
            "Test",
            vec![
                PropertySchema::new("prop1", DataType::String, None),
                PropertySchema::new("prop2", DataType::String, None),
            ],
            vec![IndexSchema::new("myindex", vec!["prop1"], false)],
            false,
        )]);
        let instance = SQLiteInstance::open(
            "test",
            Some("/Users/simon/Documents/GitHub/isar/packages/isar_core"),
            schema,
            0,
            false,
            None,
        )
        .unwrap();

        let col_id = instance.collection_ids[0];
        let mut txn = instance.begin_txn(true).unwrap();
        let mut insert = instance.insert(&mut txn, col_id, 2).unwrap();
        let mut writer = insert.get_writer().unwrap();
        writer.write_id(997);
        writer.write_string(Some("val1"));
        writer.write_string(Some("vala"));
        let mut writer = insert.insert(writer).unwrap().unwrap();
        writer.write_id(998);
        writer.write_string(Some("val2"));
        writer.write_string(Some("valb"));
        insert.insert(writer).unwrap();
        /*writer.write_id(998);
        writer.write_string(Some("val3"));
        writer.write_string(Some("val4"));
        let mut writer = insert.insert(writer).unwrap().unwrap();
        writer.write_id(999);
        writer.write_string(Some("val5"));
        writer.write_string(Some("val6"));
        insert.insert(writer).unwrap();*/
        instance.commit_txn(txn).unwrap();

        let mut txn = instance.begin_txn(false).unwrap();
        let mut qb = instance.query(col_id).unwrap();
        let filter = qb.not_null(1);
        qb.set_filter(filter);
        let q = qb.build();
        let mut cur = q.cursor(&mut txn).unwrap();
        let next = cur.next().unwrap().unwrap();
        eprintln!("{:?}", next.read_id());
        eprintln!("{:?}", next.read_string(1));
        let next = cur.next().unwrap().unwrap();
        eprintln!("{:?}", next.read_id());
        eprintln!("{:?}", next.read_string(1));
    }
}