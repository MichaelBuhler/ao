use std::env::VarError;

use dotenv::dotenv;
use futures::future::join_all;
use std::{env, io};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::interval;
use std::sync::atomic::{AtomicUsize, Ordering};

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::ConnectionManager;
use diesel::r2d2::Pool;
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

use async_trait::async_trait;

use super::super::core::dal::{
    DataStore, JsonErrorType, Message, PaginatedMessages, Process, ProcessScheduler, Scheduler,
    StoreErrorType,
};

use crate::domain::config::AoConfig;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations");

use diesel::result::Error as DieselError; // Import Diesel's Error

impl From<DieselError> for StoreErrorType {
    fn from(diesel_error: DieselError) -> Self {
        StoreErrorType::DatabaseError(format!("{:?}", diesel_error))
    }
}

impl From<serde_json::Error> for StoreErrorType {
    fn from(error: serde_json::Error) -> Self {
        StoreErrorType::JsonError(format!("data store json error: {}", error))
    }
}

impl From<JsonErrorType> for StoreErrorType {
    fn from(error: JsonErrorType) -> Self {
        StoreErrorType::JsonError(format!("data store json error: {:?}", error))
    }
}

impl From<StoreErrorType> for String {
    fn from(error: StoreErrorType) -> Self {
        format!("{:?}", error)
    }
}

impl From<String> for StoreErrorType {
    fn from(error: String) -> Self {
        StoreErrorType::DatabaseError(format!("{:?}", error))
    }
}

impl From<VarError> for StoreErrorType {
    fn from(error: VarError) -> Self {
        StoreErrorType::EnvVarError(format!("data store env var error: {}", error))
    }
}

impl From<diesel::prelude::ConnectionError> for StoreErrorType {
    fn from(error: diesel::prelude::ConnectionError) -> Self {
        StoreErrorType::DatabaseError(format!("data store connection error: {}", error))
    }
}

impl From<std::num::ParseIntError> for StoreErrorType {
    fn from(error: std::num::ParseIntError) -> Self {
        StoreErrorType::IntError(format!("data store int error: {}", error))
    }
}

pub struct StoreClient {
    pool: Pool<ConnectionManager<PgConnection>>,
    read_pool: Pool<ConnectionManager<PgConnection>>,
    use_disk: bool,
    pub bytestore: Option<bytestore::ByteStore>,
}

impl StoreClient {
    pub fn new() -> Result<Self, StoreErrorType> {
        let config = AoConfig::new(Some("su".to_string())).expect("Failed to read configuration");
        let c_clone = config.clone();
        let database_url = config.database_url;
        let database_read_url = match config.database_read_url {
            Some(u) => u,
            None => database_url.clone(),
        };
        let use_disk = config.use_disk;
        let manager = ConnectionManager::<PgConnection>::new(database_url);
        let read_manager = ConnectionManager::<PgConnection>::new(database_read_url);
        let pool = Pool::builder()
            .test_on_check_out(true)
            .build(manager)
            .map_err(|_| {
                StoreErrorType::DatabaseError("Failed to initialize connection pool.".to_string())
            })?;

        let read_pool = Pool::builder()
            .test_on_check_out(true)
            .build(read_manager)
            .map_err(|_| {
                StoreErrorType::DatabaseError(
                    "Failed to initialize read connection pool.".to_string(),
                )
            })?;

        Ok(StoreClient {
            pool,
            read_pool,
            use_disk,
            bytestore: if use_disk { 
              Some(bytestore::ByteStore::new(c_clone)) 
            } else { 
              None 
            },
        })
    }

    pub fn get_conn(
        &self,
    ) -> Result<diesel::r2d2::PooledConnection<ConnectionManager<PgConnection>>, StoreErrorType>
    {
        self.pool.get().map_err(|_| {
            StoreErrorType::DatabaseError("Failed to get connection from pool.".to_string())
        })
    }

    pub fn get_read_conn(
        &self,
    ) -> Result<diesel::r2d2::PooledConnection<ConnectionManager<PgConnection>>, StoreErrorType>
    {
        self.read_pool.get().map_err(|_| {
            StoreErrorType::DatabaseError("Failed to get connection from pool.".to_string())
        })
    }

    /*
        run at server startup to modify the database as needed
    */
    pub fn run_migrations(&self) -> Result<String, StoreErrorType> {
        let conn = &mut self.get_conn()?;
        match conn.run_pending_migrations(MIGRATIONS) {
            Ok(m) => Ok(format!("Migrations applied... {:?}", m)),
            Err(e) => Err(StoreErrorType::DatabaseError(format!(
                "Error applying migrations: {}",
                e.to_string()
            ))),
        }
    }

    pub fn get_message_count(&self) -> Result<i64, StoreErrorType> {
        use super::schema::messages::dsl::*;
        let conn = &mut self.get_read_conn()?;

        let count_result: Result<i64, DieselError> = messages.count().get_result(conn);

        match count_result {
            Ok(count) => Ok(count),
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    pub fn get_all_messages(
        &self,
        from: i64,
        to: Option<i64>,
    ) -> Result<Vec<(String, Option<String>, Vec<u8>, String, serde_json::Value, String)>, StoreErrorType>
    {
        use super::schema::messages::dsl::*;
        let conn = &mut self.get_read_conn()?;
        let mut query = messages.into_boxed();

        // Apply the offset
        query = query.offset(from);

        // Apply the limit if `to` is provided
        if let Some(to) = to {
            let limit = to - from;
            query = query.limit(limit);
        }

        let db_messages_result: Result<Vec<DbMessage>, DieselError> =
            query.order(timestamp.asc()).load(conn);

        match db_messages_result {
            Ok(db_messages) => {
                let mut messages_mapped: Vec<(
                    String,
                    Option<String>,
                    Vec<u8>,
                    String,
                    serde_json::Value,
                    String,
                )> = vec![];
                for db_message in db_messages.iter() {
                    let bytes: Vec<u8> = db_message.bundle.clone();
                    messages_mapped.push((
                        db_message.message_id.clone(),
                        db_message.assignment_id.clone(),
                        bytes,
                        db_message.process_id.clone(),
                        db_message.message_data.clone(),
                        db_message.timestamp.to_string().clone(),
                    ));
                }

                Ok(messages_mapped)
            }
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn get_message_internal(
        &self,
        message_id_in: &String,
        assignment_id_in: &Option<String>,
    ) -> Result<Message, StoreErrorType> {
        use super::schema::messages::dsl::*;
        let conn = &mut self.get_read_conn()?;

        /*
            get the oldest match. in the case of a message that has
            later assignments, it should be the original message itself.
        */
        let db_message_result: Result<Option<DbMessage>, DieselError> = match assignment_id_in {
            Some(assignment_id_d) => messages
                .filter(
                    message_id
                        .eq(message_id_in)
                        .and(assignment_id.eq(assignment_id_d)),
                )
                .order(timestamp.asc())
                .first(conn)
                .optional(),
            None => messages
                .filter(message_id.eq(message_id_in))
                .order(timestamp.asc())
                .first(conn)
                .optional(),
        };

        match db_message_result {
            Ok(Some(db_message)) => {
                let message_val: serde_json::Value =
                    serde_json::from_value(db_message.message_data.clone())?;
                let message: Message = Message::from_val(&message_val, db_message.bundle.clone())?;
                Ok(message)
            }
            Ok(None) => Err(StoreErrorType::NotFound("Message not found".to_string())), // Adjust this error type as needed
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }
  
  pub fn get_message_by_offset_from_end(&self, offset: i64) -> Result<Option<(String, Option<String>, Vec<u8>, String, serde_json::Value, String)>, StoreErrorType> {
      use super::schema::messages::dsl::*;
      let conn = &mut self.get_read_conn()?;
  
      let db_message_result: Result<Option<DbMessage>, DieselError> = messages
          .order(timestamp.desc())
          .offset(offset)
          .first(conn)
          .optional();
  
      match db_message_result {
          Ok(Some(db_message)) => {
              let bytes: Vec<u8> = db_message.bundle.clone();
              Ok(Some((
                  db_message.message_id.clone(),
                  db_message.assignment_id.clone(),
                  bytes,
                  db_message.process_id.clone(),
                  db_message.message_data.clone(),
                  db_message.timestamp.to_string().clone(),
              )))
          }
          Ok(None) => Ok(None),
          Err(e) => Err(StoreErrorType::from(e)),
      }
  }
  
  /*
    Start at the end of the messages table, scan
    backwards and insert messages into the bytestore
    if they dont exist.
  */
  pub fn sync_bytestore(&self) -> Result<(), ()> {
      println!("Syncing the tail of the messages table");
      use std::time::Instant;
      let start = Instant::now();

      let total_count = self.get_message_count().expect("Failed to get message count");
      let mut synced_count = 0;

      for offset in 0..total_count {
          let result = self.get_message_by_offset_from_end(offset);

          match result {
              Ok(Some(message)) => {
                  let msg_id = message.0;
                  let assignment_id = message.1;
                  let bundle = message.2;
                  let process_id = message.3;
                  let timestamp = message.5;

                  if self.bytestore.clone().unwrap()
                      .exists(&msg_id, &assignment_id, &process_id, &timestamp) {
                          // Stop the migration if message is already in byte store
                          let duration = start.elapsed();
                          println!("Time elapsed in sync is: {:?}", duration);
                          println!("Number of messages synced: {}", synced_count);
                          return Ok(());
                  }

                  self.bytestore.clone().unwrap()
                      .save_binary(
                          msg_id.clone(),
                          assignment_id.clone(),
                          process_id.clone(),
                          timestamp.clone(),
                          bundle,
                      )
                      .expect("Failed to save message binary");

                  synced_count += 1;
              }
              Ok(None) => {
                  println!("No more messages to process.");
                  break;
              }
              Err(e) => {
                  eprintln!("Error fetching messages: {:?}", e);
              }
          }
      }

      let duration = start.elapsed();
      println!("Time elapsed in sync is: {:?}", duration);
      println!("Number of messages synced: {}", synced_count);

      Ok(())
  }

}

#[async_trait]
impl DataStore for StoreClient {
    fn save_process(&self, process: &Process, bundle_in: &[u8]) -> Result<String, StoreErrorType> {
        use super::schema::processes::dsl::*;
        let conn = &mut self.get_conn()?;

        let new_process = NewProcess {
            process_id: &process.process_id,
            process_data: serde_json::to_value(process).expect("Failed to serialize Process"),
            bundle: bundle_in,
        };

        match diesel::insert_into(processes)
            .values(&new_process)
            .on_conflict(process_id)
            .do_nothing()
            .execute(conn)
        {
            Ok(_) => {
              Ok("saved".to_string())
            },
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn get_process(&self, process_id_in: &str) -> Result<Process, StoreErrorType> {
        use super::schema::processes::dsl::*;
        let conn = &mut self.get_read_conn()?;

        let db_process_result: Result<Option<DbProcess>, DieselError> = processes
            .filter(process_id.eq(process_id_in))
            .first(conn)
            .optional();

        match db_process_result {
            Ok(Some(db_process)) => {
                let process: Process = serde_json::from_value(db_process.process_data.clone())?;
                Ok(process)
            }
            Ok(None) => Err(StoreErrorType::NotFound("Process not found".to_string())),
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    /*
        If we are trying to write an actual data item
        not just an assignment we need to check that it
        doesnt already exist.
    */
    fn check_existing_message(&self, message: &Message) -> Result<(), StoreErrorType> {
        match &message.message {
            Some(m) => {
                match self.get_message(&m.id) {
                    Ok(parsed) => {
                        /*
                            If the message already exists and it contains
                            an actual message (it is not just an assignment)
                            then throw an error to avoid duplicate data items
                            being written
                        */
                        match parsed.message {
                            Some(_) => Err(StoreErrorType::MessageExists(
                                "Message already exists".to_string(),
                            )),
                            None => Ok(()),
                        }
                    }
                    // The message wasnt found at all so it can be written
                    Err(StoreErrorType::NotFound(_)) => Ok(()),
                    // Some other error happened
                    Err(_) => Err(StoreErrorType::DatabaseError(
                        "Error checking message".to_string(),
                    )),
                }
            }
            None => Ok(()),
        }
    }

    async fn save_message(
        &self,
        message: &Message,
        bundle_in: &[u8],
    ) -> Result<String, StoreErrorType> {
        use super::schema::messages::dsl::*;
        let conn = &mut self.get_conn()?;

        self.check_existing_message(message)?;

        let new_message = NewMessage {
            process_id: &message.process_id()?,
            message_id: &message.message_id()?,
            assignment_id: &message.assignment_id()?,
            message_data: serde_json::to_value(message).expect("Failed to serialize Message"),
            epoch: &message.epoch()?,
            nonce: &message.nonce()?,
            timestamp: &message.timestamp()?,
            bundle: bundle_in,
            hash_chain: &message.hash_chain()?,
        };

        match diesel::insert_into(messages)
            .values(&new_message)
            .execute(conn)
        {
            Ok(row_count) => {
                if row_count == 0 {
                    Err(StoreErrorType::DatabaseError(
                        "Error saving message".to_string(),
                    )) // Return a custom error for duplicates
                } else {
                    if self.use_disk {
                      self.bytestore.clone().ok_or("Error: bytestore is None".to_string())?
                        .save_binary(
                          message.message_id()?,
                          Some(message.assignment_id()?),
                          message.process_id()?,
                          message.timestamp()?.to_string(),
                          bundle_in.to_vec(),
                        )?;
                    }
                    Ok("saved".to_string())
                }
            }
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    async fn get_messages(
        &self,
        process_id_in: &str,
        from: &Option<String>,
        to: &Option<String>,
        limit: &Option<i32>,
    ) -> Result<PaginatedMessages, StoreErrorType> {
        use super::schema::messages::dsl::*;
        let conn = &mut self.get_read_conn()?;
        let mut query = messages.filter(process_id.eq(process_id_in)).into_boxed();

        // Apply 'from' timestamp filtering if 'from' is provided
        if let Some(from_timestamp_str) = from {
            let from_timestamp = from_timestamp_str
                .parse::<i64>()
                .map_err(StoreErrorType::from)?;
            query = query.filter(timestamp.gt(from_timestamp));
        }

        // Apply 'to' timestamp filtering if 'to' is provided
        if let Some(to_timestamp_str) = to {
            let to_timestamp = to_timestamp_str
                .parse::<i64>()
                .map_err(StoreErrorType::from)?;
            query = query.filter(timestamp.le(to_timestamp));
        }

        // Apply limit, converting Option<i32> to i64 and adding 1 to check for the next page
        let limit_val = limit.unwrap_or(5000) as i64; // Default limit if none is provided

        if self.use_disk {
            let db_messages_result: Result<Vec<DbMessageWithoutData>, DieselError> = query
                .select((
                    row_id,
                    process_id,
                    message_id,
                    assignment_id,
                    epoch,
                    nonce,
                    timestamp,
                    hash_chain,
                ))
                .order(timestamp.asc())
                .limit(limit_val + 1) // Fetch one extra record to determine if a next page exists
                .load(conn);

            match db_messages_result {
                Ok(db_messages) => {
                    let has_next_page = db_messages.len() as i64 > limit_val;
                    // Take only up to the limit if there's an extra indicating a next page
                    let messages_o = if has_next_page {
                        &db_messages[..(limit_val as usize)]
                    } else {
                        &db_messages[..]
                    };

                    let message_ids: Vec<(String, Option<String>, String, String)> = messages_o
                        .iter()
                        .map(|msg| {
                            (
                                msg.message_id.clone(),
                                msg.assignment_id.clone(),
                                msg.process_id.clone(),
                                msg.timestamp.to_string().clone(),
                            )
                        })
                        .collect();

                    let binaries = self.bytestore
                      .clone()
                      .ok_or("Bytestore is empty".to_string())?
                      .read_binaries(message_ids)
                      .await?;

                    let mut messages_mapped: Vec<Message> = vec![];

                    for db_message in messages_o.iter() {
                        /*
                          binaries is keyed by the tuple (message_id, assignment_id, process_id, timestamp)
                        */
                        match binaries.get(&(
                            db_message.message_id.clone(),
                            db_message.assignment_id.clone(),
                            db_message.process_id.clone(),
                            db_message.timestamp.to_string().clone(),
                        )) {
                            Some(bytes_result) => {
                                let mapped = Message::from_bytes(bytes_result.clone())?;
                                messages_mapped.push(mapped);
                            }
                            None => {
                                /*
                                  If for some reason we dont have a file available
                                  this is a fall back to the database
                                */
                                let full_message = self.get_message_internal(
                                    &db_message.message_id,
                                    &db_message.assignment_id,
                                )?;
                                messages_mapped.push(full_message);
                            }
                        }
                    }
                    let paginated =
                        PaginatedMessages::from_messages(messages_mapped, has_next_page)?;
                    Ok(paginated)
                }
                Err(e) => Err(StoreErrorType::from(e)),
            }
        } else {
            let db_messages_result: Result<Vec<DbMessage>, DieselError> = query
                .order(timestamp.asc())
                .limit(limit_val + 1) // Fetch one extra record to determine if a next page exists
                .load(conn);

            match db_messages_result {
                Ok(db_messages) => {
                    let has_next_page = db_messages.len() as i64 > limit_val;
                    // Take only up to the limit if there's an extra indicating a next page
                    let messages_o = if has_next_page {
                        &db_messages[..(limit_val as usize)]
                    } else {
                        &db_messages[..]
                    };

                    let mut messages_mapped: Vec<Message> = vec![];
                    for db_message in messages_o.iter() {
                        let json = serde_json::from_value(db_message.message_data.clone())?;
                        let bytes: Vec<u8> = db_message.bundle.clone();
                        let mapped = Message::from_val(&json, bytes)?;
                        messages_mapped.push(mapped);
                    }

                    let paginated =
                        PaginatedMessages::from_messages(messages_mapped, has_next_page)?;
                    Ok(paginated)
                }
                Err(e) => Err(StoreErrorType::from(e)),
            }
        }
    }

    fn get_message(&self, tx_id: &str) -> Result<Message, StoreErrorType> {
        use super::schema::messages::dsl::*;
        let conn = &mut self.get_read_conn()?;

        /*
            get the oldest match. in the case of a message that has
            later assignments, it should be the original message itself.
        */
        let db_message_result: Result<Option<DbMessage>, DieselError> = messages
            .filter(message_id.eq(tx_id).or(assignment_id.eq(tx_id)))
            .order(timestamp.asc())
            .first(conn)
            .optional();

        match db_message_result {
            Ok(Some(db_message)) => {
                let message_val: serde_json::Value =
                    serde_json::from_value(db_message.message_data.clone())?;
                let message: Message = Message::from_val(&message_val, db_message.bundle.clone())?;
                Ok(message)
            }
            Ok(None) => Err(StoreErrorType::NotFound("Message not found".to_string())), // Adjust this error type as needed
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn get_latest_message(&self, process_id_in: &str) -> Result<Option<Message>, StoreErrorType> {
        use super::schema::messages::dsl::*;
        /*
            This must use get_conn because it needs
            an up to date record from the writer instance
            it cannot be behind at all as it is used
            in the scheduling process.
        */
        let conn = &mut self.get_conn()?;

        // Get the latest DbMessage
        let latest_db_message_result = messages
            .filter(process_id.eq(process_id_in))
            .order(row_id.desc())
            .first::<DbMessage>(conn);

        match latest_db_message_result {
            Ok(db_message) => {
                // Deserialize the message_data into Message
                let message_val: serde_json::Value =
                    serde_json::from_value(db_message.message_data)
                        .map_err(|e| StoreErrorType::from(e))?;

                let message: Message = Message::from_val(&message_val, db_message.bundle.clone())?;

                Ok(Some(message))
            }
            Err(DieselError::NotFound) => Ok(None), // No messages found
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn save_process_scheduler(
        &self,
        process_scheduler: &ProcessScheduler,
    ) -> Result<String, StoreErrorType> {
        use super::schema::process_schedulers::dsl::*;
        let conn = &mut self.get_conn()?;

        let new_process_scheduler = NewProcessScheduler {
            process_id: &process_scheduler.process_id,
            scheduler_row_id: &process_scheduler.scheduler_row_id,
        };

        match diesel::insert_into(process_schedulers)
            .values(&new_process_scheduler)
            .on_conflict(process_id)
            .do_nothing()
            .execute(conn)
        {
            Ok(_) => Ok("saved".to_string()),
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn get_process_scheduler(
        &self,
        process_id_in: &str,
    ) -> Result<ProcessScheduler, StoreErrorType> {
        use super::schema::process_schedulers::dsl::*;
        let conn = &mut self.get_read_conn()?;

        let db_process_result: Result<Option<DbProcessScheduler>, DieselError> = process_schedulers
            .filter(process_id.eq(process_id_in))
            .first(conn)
            .optional();

        match db_process_result {
            Ok(Some(db_process_scheduler)) => {
                let process_scheduler: ProcessScheduler = ProcessScheduler {
                    row_id: Some(db_process_scheduler.row_id),
                    process_id: db_process_scheduler.process_id,
                    scheduler_row_id: db_process_scheduler.scheduler_row_id,
                };
                Ok(process_scheduler)
            }
            Ok(None) => Err(StoreErrorType::NotFound(
                "Process scheduler not found".to_string(),
            )),
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn save_scheduler(&self, scheduler: &Scheduler) -> Result<String, StoreErrorType> {
        use super::schema::schedulers::dsl::*;
        let conn = &mut self.get_conn()?;

        let new_scheduler = NewScheduler {
            url: &scheduler.url,
            process_count: &scheduler.process_count,
        };

        match diesel::insert_into(schedulers)
            .values(&new_scheduler)
            .on_conflict(url)
            .do_nothing()
            .execute(conn)
        {
            Ok(_) => Ok("saved".to_string()),
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn update_scheduler(&self, scheduler: &Scheduler) -> Result<String, StoreErrorType> {
        use super::schema::schedulers::dsl::*;
        let conn = &mut self.get_conn()?;

        // Ensure scheduler.row_id is Some(value) before calling this function
        match diesel::update(schedulers.filter(row_id.eq(scheduler.row_id.unwrap())))
            .set((
                process_count.eq(scheduler.process_count),
                url.eq(&scheduler.url),
            ))
            .execute(conn)
        {
            Ok(_) => Ok("updated".to_string()),
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn get_scheduler(&self, row_id_in: &i32) -> Result<Scheduler, StoreErrorType> {
        use super::schema::schedulers::dsl::*;
        let conn = &mut self.get_read_conn()?;

        let db_scheduler_result: Result<Option<DbScheduler>, DieselError> = schedulers
            .filter(row_id.eq(row_id_in))
            .first(conn)
            .optional();

        match db_scheduler_result {
            Ok(Some(db_scheduler)) => {
                let scheduler: Scheduler = Scheduler {
                    row_id: Some(db_scheduler.row_id),
                    url: db_scheduler.url,
                    process_count: db_scheduler.process_count,
                };
                Ok(scheduler)
            }
            Ok(None) => Err(StoreErrorType::NotFound("Scheduler not found".to_string())),
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn get_scheduler_by_url(&self, url_in: &String) -> Result<Scheduler, StoreErrorType> {
        use super::schema::schedulers::dsl::*;
        let conn = &mut self.get_read_conn()?;

        let db_scheduler_result: Result<Option<DbScheduler>, DieselError> =
            schedulers.filter(url.eq(url_in)).first(conn).optional();

        match db_scheduler_result {
            Ok(Some(db_scheduler)) => {
                let scheduler: Scheduler = Scheduler {
                    row_id: Some(db_scheduler.row_id),
                    url: db_scheduler.url,
                    process_count: db_scheduler.process_count,
                };
                Ok(scheduler)
            }
            Ok(None) => Err(StoreErrorType::NotFound("Scheduler not found".to_string())),
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }

    fn get_all_schedulers(&self) -> Result<Vec<Scheduler>, StoreErrorType> {
        use super::schema::schedulers::dsl::*;
        let conn = &mut self.get_read_conn()?;

        match schedulers.order(row_id.asc()).load::<DbScheduler>(conn) {
            Ok(db_schedulers) => {
                let schedulers_out: Vec<Scheduler> = db_schedulers
                    .into_iter()
                    .map(|db_scheduler| Scheduler {
                        row_id: Some(db_scheduler.row_id),
                        url: db_scheduler.url,
                        process_count: db_scheduler.process_count,
                    })
                    .collect();
                Ok(schedulers_out)
            }
            Err(e) => Err(StoreErrorType::from(e)),
        }
    }
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = super::schema::processes)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DbProcess {
    pub row_id: i32,
    pub process_id: String,
    pub process_data: serde_json::Value,
    pub bundle: Vec<u8>,
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = super::schema::messages)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DbMessage {
    pub row_id: i32,
    pub process_id: String,
    pub message_id: String,
    pub assignment_id: Option<String>,
    pub message_data: serde_json::Value,
    pub epoch: i32,
    pub nonce: i32,
    pub timestamp: i64,
    pub bundle: Vec<u8>,
    pub hash_chain: String,
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = super::schema::messages)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DbMessageWithoutData {
    pub row_id: i32,
    pub process_id: String,
    pub message_id: String,
    pub assignment_id: Option<String>,
    pub epoch: i32,
    pub nonce: i32,
    pub timestamp: i64,
    pub hash_chain: String,
}

#[derive(Insertable)]
#[diesel(table_name = super::schema::messages)]
pub struct NewMessage<'a> {
    pub process_id: &'a str,
    pub message_id: &'a str,
    pub assignment_id: &'a str,
    pub message_data: serde_json::Value,
    pub bundle: &'a [u8],
    pub epoch: &'a i32,
    pub nonce: &'a i32,
    pub timestamp: &'a i64,
    pub hash_chain: &'a str,
}

#[derive(Insertable)]
#[diesel(table_name = super::schema::processes)]
pub struct NewProcess<'a> {
    pub process_id: &'a str,
    pub process_data: serde_json::Value,
    pub bundle: &'a [u8],
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = super::schema::schedulers)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DbScheduler {
    pub row_id: i32,
    pub url: String,
    pub process_count: i32,
}

#[derive(Insertable)]
#[diesel(table_name = super::schema::schedulers)]
pub struct NewScheduler<'a> {
    pub url: &'a str,
    pub process_count: &'a i32,
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = super::schema::process_schedulers)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DbProcessScheduler {
    pub row_id: i32,
    pub process_id: String,
    pub scheduler_row_id: i32,
}

#[derive(Insertable)]
#[diesel(table_name = super::schema::process_schedulers)]
pub struct NewProcessScheduler<'a> {
    pub process_id: &'a str,
    pub scheduler_row_id: &'a i32,
}

/*
  A simple module for storing and retrieving files using
  the disk. This is implemented using RocksDB in BlobDB mode.

  See https://rocksdb.org/blog/2021/05/26/integrated-blob-db.html
*/
mod bytestore {
  use dashmap::DashMap;
  use rocksdb::{DB, Options};
  use std::sync::Arc;
  use super::super::super::config::AoConfig;

  #[derive(Clone)]
  pub struct ByteStore {
      db: Arc<DB>,
  }

  impl ByteStore {
      pub fn new(config: AoConfig) -> Self {
          let mut opts = Options::default();
          opts.create_if_missing(true);
          opts.set_enable_blob_files(true); // Enable blob files
          opts.set_blob_file_size(5 * 1024 * 1024 * 1024); // 5GB max for now
          opts.set_min_blob_size(1024); // low value ensures it is used

          let db = DB::open(&opts, &config.su_data_dir).expect("Failed to open RocksDB");

          ByteStore {
              db: Arc::new(db),
          }
      }

      pub async fn read_binaries(
          &self,
          ids: Vec<(String, Option<String>, String, String)>,
      ) -> Result<DashMap<(String, Option<String>, String, String), Vec<u8>>, String> {
          let binaries = Arc::new(DashMap::new());
          let db = self.db.clone();

          for id in ids {
              let db = db.clone();
              let binaries = binaries.clone();

              let key = ByteStore::create_key(&id.0, &id.1, &id.2, &id.3);
              if let Ok(Some(value)) = db.get(&key) {
                  binaries.insert(id.clone(), value);
              }
          }

          Ok(Arc::try_unwrap(binaries).map_err(|_| "Failed to unwrap Arc")?)
      }

      pub fn save_binary(
          &self,
          message_id: String,
          assignment_id: Option<String>,
          process_id: String,
          timestamp: String,
          binary: Vec<u8>,
      ) -> Result<(), String> {
          let key = ByteStore::create_key(&message_id, &assignment_id, &process_id, &timestamp);
          self
            .db
            .put(key, binary)
            .map_err(|e| format!("Failed to write to RocksDB: {:?}", e))?;
          Ok(())
      }

      fn create_key(
          message_id: &str,
          assignment_id: &Option<String>,
          process_id: &str,
          timestamp: &str,
      ) -> Vec<u8> {
          match assignment_id {
              Some(assignment_id) => format!("message___{}___{}___{}___{}", process_id, timestamp, message_id, assignment_id).into_bytes(),
              None => format!("message___{}___{}___{}", process_id, timestamp, message_id).into_bytes(),
          }
      }

      pub fn exists(
          &self,
          message_id: &str,
          assignment_id: &Option<String>,
          process_id: &str,
          timestamp: &str,
      ) -> bool {
          let key = ByteStore::create_key(message_id, assignment_id, process_id, timestamp);
          match self.db.get(&key) {
              Ok(Some(_)) => true,
              _ => false,
          }
      }
  }
}




/*
  This function is used by the migration binary
  to move all data from the database to the disk.
  It is not meant to be run anywhere within the su
  server itself.
*/
pub async fn migrate_to_disk() -> io::Result<()> {
  use std::time::{Instant, Duration};
  let start = Instant::now();
  dotenv().ok();

  let data_store = Arc::new(StoreClient::new().expect("Failed to create StoreClient"));

  let args: Vec<String> = env::args().collect();
  let range: &String = args.get(1).expect("Range argument not provided");

  let (from, to) = parse_range(range);

  let total_count = match to {
      Some(t) => {
          let total = data_store
              .get_message_count()
              .expect("Failed to get message count");
          if t > total {
              total - from
          } else {
              t - from
          }
      },
      None => {
          data_store
              .get_message_count()
              .expect("Failed to get message count")
              - from
      }
  };

  println!("Total messages to process: {}", total_count);

  let config = AoConfig::new(Some("su".to_string())).expect("Failed to read configuration");
  let batch_size = config.migration_batch_size.clone() as usize;

  let processed_count = Arc::new(AtomicUsize::new(0));

  // Spawn a task to log progress every minute
  let processed_count_clone = Arc::clone(&processed_count);
  tokio::spawn(async move {
      let mut interval = interval(Duration::from_secs(10));
      loop {
          interval.tick().await;
          println!("Messages processed update: {}", processed_count_clone.load(Ordering::SeqCst));
          if processed_count_clone.load(Ordering::SeqCst) >= total_count as usize {
              break;
          }
      }
  });

  for batch_start in (from..from + total_count).step_by(batch_size) {
      let batch_end = if let Some(t) = to {
          std::cmp::min(batch_start + batch_size as i64, t)
      } else {
          batch_start + batch_size as i64
      };

      let data_store = Arc::clone(&data_store);
      let processed_count = Arc::clone(&processed_count);

      let result = data_store.get_all_messages(batch_start, Some(batch_end));

      match result {
          Ok(messages) => {
              let mut save_handles: Vec<JoinHandle<()>> = Vec::new();
              for message in messages {
                  let msg_id = message.0;
                  let assignment_id = message.1;
                  let bundle = message.2;
                  let process_id = message.3;
                  let timestamp = message.5;
                  let data_store = Arc::clone(&data_store);
                  let processed_count = Arc::clone(&processed_count);

                  let handle = tokio::spawn(async move {
                      data_store.bytestore.clone().expect("Bytestore is None")
                          .save_binary(
                              msg_id.clone(),
                              assignment_id.clone(),
                              process_id.clone(),
                              timestamp.clone(),
                              bundle,
                          )
                          .expect("Failed to save message binary");
                      processed_count.fetch_add(1, Ordering::SeqCst);
                  });

                  save_handles.push(handle);
              }
              join_all(save_handles).await;
          }
          Err(e) => {
              eprintln!("Error fetching messages: {:?}", e);
          }
      }
  }

  let duration = start.elapsed();
  println!("Time elapsed in data migration is: {:?}", duration);

  Ok(())
}


fn parse_range(range: &str) -> (i64, Option<i64>) {
  let parts: Vec<&str> = range.split('-').collect();
  let from = parts[0].parse().expect("Invalid starting offset");
  let to = if parts.len() > 1 {
      Some(parts[1].parse().expect("Invalid records to pull"))
  } else {
      None
  };
  (from, to)
}