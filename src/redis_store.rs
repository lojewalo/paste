use diesel::r2d2::{Pool, PooledConnection};

use r2d2_redis::RedisConnectionManager;

use redis::Connection;

use rocket::http::Status;
use rocket::request::{self, FromRequest};
use rocket::{Request, State, Outcome};

use std::env;
use std::ops::Deref;

pub type RedisPool = Pool<RedisConnectionManager>;

lazy_static! {
  static ref REDIS_URL: String = env::var("REDIS_URL").expect("missing REDIS_URL env var");
}

pub fn init_pool() -> RedisPool {
  let manager = RedisConnectionManager::new(REDIS_URL.as_str()).expect("could not connect to redis");
  Pool::new(manager).expect("redis pool")
}

pub struct Redis(pub PooledConnection<RedisConnectionManager>);

impl<'a, 'r> FromRequest<'a, 'r> for Redis {
  type Error = ();

  fn from_request(request: &'a Request<'r>) -> request::Outcome<Self, Self::Error> {
    let pool: State<RedisPool> = request.guard()?;
    match pool.get() {
      Ok(conn) => Outcome::Success(Redis(conn)),
      Err(_) => Outcome::Failure((Status::ServiceUnavailable, ())),
    }
  }
}

impl Deref for Redis {
  type Target = Connection;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}
