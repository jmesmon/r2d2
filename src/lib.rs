//! A library providing a generic connection pool.
#![feature(unsafe_destructor, core, std_misc)]
#![warn(missing_docs)]
#![doc(html_root_url="https://sfackler.github.io/r2d2/doc")]

#[macro_use]
extern crate log;
extern crate time;

use std::collections::RingBuf;
use std::error::Error;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, Condvar};
use std::time::Duration;
use time::SteadyTime;

#[doc(inline)]
pub use config::Config;

use task::ScheduledThreadPool;

pub mod config;
mod task;

/// A trait which provides connection-specific functionality.
pub trait ConnectionManager: Send+Sync {
    type Connection: Send;
    type Error;

    /// Attempts to create a new connection.
    fn connect(&self) -> Result<Self::Connection, Self::Error>;

    /// Determines if the connection is still connected to the database.
    ///
    /// A standard implementation would check if a simple query like `SELECT 1`
    /// succeeds.
    fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error>;

    /// *Quickly* determines if the connection is no longer usable.
    ///
    /// This will be called synchronously every time a connection is returned
    /// to the pool, so it should *not* block. If it returns `true`, the
    /// connection will be discarded.
    ///
    /// For example, an implementation might check if the underlying TCP socket
    /// has disconnected. Implementations that do not support this kind of
    /// fast health check may simply return `false`.
    fn has_broken(&self, conn: &mut Self::Connection) -> bool;
}

/// A trait which handles errors reported by the `ConnectionManager`.
pub trait ErrorHandler<E>: Send+Sync {
    /// Handles an error.
    fn handle_error(&self, error: E);
}

/// An `ErrorHandler` which does nothing.
#[derive(Copy, Clone, Debug)]
pub struct NoopErrorHandler;

impl<E> ErrorHandler<E> for NoopErrorHandler {
    fn handle_error(&self, _: E) {}
}

/// An `ErrorHandler` which logs at the error level.
#[derive(Copy, Clone, Debug)]
pub struct LoggingErrorHandler;

impl<E> ErrorHandler<E> for LoggingErrorHandler where E: fmt::Debug {
    fn handle_error(&self, error: E) {
        error!("{:?}", error);
    }
}

struct PoolInternals<C> {
    conns: RingBuf<C>,
    num_conns: u32,
}

struct SharedPool<M> where M: ConnectionManager {
    config: Config,
    manager: M,
    error_handler: Box<ErrorHandler<<M as ConnectionManager>::Error>>,
    internals: Mutex<PoolInternals<<M as ConnectionManager>::Connection>>,
    cond: Condvar,
    thread_pool: ScheduledThreadPool,
}

fn add_connection<M>(delay: Duration, shared: &Arc<SharedPool<M>>) where M: ConnectionManager {
    let new_shared = shared.clone();
    shared.thread_pool.run_after(delay, move || {
        let shared = new_shared;
        match shared.manager.connect() {
            Ok(conn) => {
                let mut internals = shared.internals.lock().unwrap();
                internals.conns.push_back(conn);
                internals.num_conns += 1;
                shared.cond.notify_one();
            }
            Err(err) => {
                shared.error_handler.handle_error(err);
                add_connection(Duration::seconds(1), &shared);
            },
        }
    });
}

/// A generic connection pool.
pub struct Pool<M> where M: ConnectionManager {
    shared: Arc<SharedPool<M>>,
}

#[unsafe_destructor]
impl<M> Drop for Pool<M> where M: ConnectionManager {
    fn drop(&mut self) {
        self.shared.thread_pool.clear();
    }
}

impl<M> fmt::Debug for Pool<M> where M: ConnectionManager + fmt::Debug {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Pool {{ idle_connections: {}, config: {:?}, manager: {:?} }}",
               self.shared.internals.lock().unwrap().conns.len(),
               self.shared.config,
               self.shared.manager)
    }
}

/// An error returned by `Pool::new` if it fails to initialize connections.
#[derive(Debug)]
pub struct InitializationError;

impl fmt::Display for InitializationError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str(self.description())
    }
}

impl Error for InitializationError {
    fn description(&self) -> &str {
        "Unable to initialize connections"
    }
}

/// An error returned by `Pool::get` if it times out without retrieving a connection.
#[derive(Debug)]
pub struct GetTimeout;

impl fmt::Display for GetTimeout {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str(self.description())
    }
}

impl Error for GetTimeout {
    fn description(&self) -> &str {
        "Timed out while waiting for a connection"
    }
}

impl<M> Pool<M> where M: ConnectionManager {
    /// Creates a new connection pool.
    ///
    /// Returns an `Err` value if `initialization_fail_fast` is set to true in
    /// the configuration and the pool is unable to open all of its
    /// connections.
    pub fn new(config: Config,
               manager: M,
               error_handler: Box<ErrorHandler<<M as ConnectionManager>::Error>>)
               -> Result<Pool<M>, InitializationError> {
        let internals = PoolInternals {
            conns: RingBuf::new(),
            num_conns: 0,
        };

        let shared = Arc::new(SharedPool {
            config: config,
            manager: manager,
            error_handler: error_handler,
            internals: Mutex::new(internals),
            cond: Condvar::new(),
            thread_pool: ScheduledThreadPool::new(config.helper_threads() as usize),
        });

        for _ in 0..config.pool_size() {
            add_connection(Duration::zero(), &shared);
        }

        if shared.config.initialization_fail_fast() {
            let internals = shared.internals.lock().unwrap();
            let initialized = shared.cond.wait_timeout_with(internals,
                                                            shared.config.connection_timeout(),
                                                            |internals| {
                internals.unwrap().num_conns == shared.config.pool_size()
            }).unwrap().1;

            if !initialized {
                return Err(InitializationError);
            }
        }

        Ok(Pool {
            shared: shared,
        })
    }

    /// Retrieves a connection from the pool.
    ///
    /// Waits for at most `Config::connection_timeout` before returning an
    /// error.
    pub fn get<'a>(&'a self) -> Result<PooledConnection<'a, M>, GetTimeout> {
        let end = SteadyTime::now() + self.shared.config.connection_timeout();
        let mut internals = self.shared.internals.lock().unwrap();

        loop {
            match internals.conns.pop_front() {
                Some(mut conn) => {
                    drop(internals);

                    if self.shared.config.test_on_check_out() {
                        if let Err(e) = self.shared.manager.is_valid(&mut conn) {
                            self.shared.error_handler.handle_error(e);
                            internals = self.shared.internals.lock().unwrap();
                            internals.num_conns -= 1;
                            add_connection(Duration::zero(), &self.shared);
                            continue
                        }
                    }

                    return Ok(PooledConnection {
                        pool: self,
                        conn: Some(conn),
                    })
                }
                None => {
                    let now = SteadyTime::now();
                    let (new_internals, no_timeout) =
                        self.shared.cond.wait_timeout(internals, end - now).unwrap();
                    internals = new_internals;

                    if !no_timeout {
                        return Err(GetTimeout);
                    }
                }
            }
        }
    }

    fn put_back(&self, mut conn: <M as ConnectionManager>::Connection) {
        // This is specified to be fast, but call it before locking anyways
        let broken = self.shared.manager.has_broken(&mut conn);

        let mut internals = self.shared.internals.lock().unwrap();
        if broken {
            internals.num_conns -= 1;
        } else {
            internals.conns.push_back(conn);
            self.shared.cond.notify_one();
        }
    }
}

/// A smart pointer wrapping a connection.
pub struct PooledConnection<'a, M> where M: ConnectionManager {
    pool: &'a Pool<M>,
    conn: Option<<M as ConnectionManager>::Connection>,
}

impl<'a, M> fmt::Debug for PooledConnection<'a, M>
        where M: ConnectionManager + fmt::Debug,
        <M as ConnectionManager>::Connection: fmt::Debug {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "PooledConnection {{ pool: {:?}, connection: {:?} }}", self.pool,
               self.conn.as_ref().unwrap())
    }
}

#[unsafe_destructor]
impl<'a, M> Drop for PooledConnection<'a, M> where M: ConnectionManager {
    fn drop(&mut self) {
        self.pool.put_back(self.conn.take().unwrap());
    }
}

impl<'a, M> Deref for PooledConnection<'a, M> where M: ConnectionManager {
    type Target = <M as ConnectionManager>::Connection;

    fn deref(&self) -> &<M as ConnectionManager>::Connection {
        self.conn.as_ref().unwrap()
    }
}

impl<'a, M> DerefMut for PooledConnection<'a, M> where M: ConnectionManager {
    fn deref_mut(&mut self) -> &mut <M as ConnectionManager>::Connection {
        self.conn.as_mut().unwrap()
    }
}
