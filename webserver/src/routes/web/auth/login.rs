use crate::{
  config::Config,
  database::{
    DbConn,
    models::{backup_codes::BackupCode, login_attempts::LoginAttempt, users::User},
    schema::{backup_codes, users},
  },
  errors::*,
  i18n::prelude::*,
  redis_store::Redis,
  routes::web::{context, AddCsp, Honeypot, Rst, OptionalWebUser, Session, auth::PotentialUser},
  utils::{
    AcceptLanguage,
    ClientIp,
    totp::totp_raw_skew,
  },
};

use diesel::prelude::*;

use r2d2_redis::redis::Commands;

use rocket::{
  State,
  http::Cookies,
  request::Form,
  response::Redirect,
};

use rocket_contrib::templates::Template;

use serde_json::json;

#[get("/login")]
pub fn get(config: State<Config>, user: OptionalWebUser, mut sess: Session, langs: AcceptLanguage) -> AddCsp<Rst> {
  if user.is_some() {
    return AddCsp::none(Rst::Redirect(Redirect::to("lastpage")));
  }

  let honeypot = Honeypot::new();
  let mut ctx = context(&*config, user.as_ref(), &mut sess, langs);
  ctx["honeypot"] = json!(honeypot);
  ctx["links"] = json!(links!(
    "login_action" => uri!(crate::routes::web::auth::login::post),
    "forgot_password" => uri!(crate::routes::web::account::reset_password::get),
  ));
  AddCsp::new(
    Rst::Template(Template::render("auth/login", ctx)),
    vec![format!("style-src '{}'", honeypot.integrity_hash)],
  )
}

#[derive(Debug, FromForm, Serialize)]
pub struct RegistrationData {
  username: String,
  #[serde(skip)]
  password: String,
  #[serde(skip)]
  anti_csrf_token: String,
  #[serde(skip)]
  #[form(field = "email")]
  honeypot: String,
}

#[post("/login", format = "application/x-www-form-urlencoded", data = "<data>")]
pub fn post(data: Form<RegistrationData>, mut sess: Session, conn: DbConn, mut redis: Redis, mut cookies: Cookies, addr: ClientIp, l10n: L10n) -> Result<Redirect> {
  let data = data.into_inner();
  sess.set_form(&data);

  if !sess.check_token(&data.anti_csrf_token) {
    sess.add_data("error", l10n.tr("error-csrf")?);
    return Ok(Redirect::to(uri!(crate::routes::web::auth::login::get)));
  }

  if !data.honeypot.is_empty() {
    sess.add_data("error", l10n.tr(("antispam-honeypot", "error"))?);
    return Ok(Redirect::to(uri!(crate::routes::web::auth::login::get)));
  }

  if let Some(msg) = LoginAttempt::find_check(&conn, &l10n, *addr)? {
    sess.add_data("error", msg);
    return Ok(Redirect::to(uri!(crate::routes::web::auth::login::get)));
  }

  let user: Option<User> = users::table
    .filter(users::username.eq(&data.username))
    .first(&*conn)
    .optional()?;

  let user = match user {
    Some(u) => u,
    None => {
      let msg = match LoginAttempt::find_increment(&conn, &l10n, *addr)? {
        Some(msg) => msg,
        None => l10n.tr(("login-error", "username"))?,
      };
      sess.add_data("error", msg);
      return Ok(Redirect::to(uri!(crate::routes::web::auth::login::get)));
    },
  };

  if !user.check_password(&data.password) {
    let msg = match LoginAttempt::find_increment(&conn, &l10n, *addr)? {
      Some(msg) => msg,
      None => l10n.tr(("login-error", "password"))?,
    };
    sess.add_data("error", msg);
    return Ok(Redirect::to(uri!(crate::routes::web::auth::login::get)));
  }

  sess.take_form();

  if user.tfa_enabled() {
    PotentialUser::set(&mut redis, &mut cookies, user.id())?;
    return Ok(Redirect::to(uri!(tfa)));
  }

  sess.user_id = Some(user.id());

  Ok(Redirect::to("lastpage"))
}

#[get("/login/2fa")]
pub fn tfa(config: State<Config>, user: OptionalWebUser, pot: Option<PotentialUser>, mut sess: Session, langs: AcceptLanguage) -> Rst {
  if user.is_some() || pot.is_none() {
    return Rst::Redirect(Redirect::to("lastpage"));
  }

  let mut ctx = context(&*config, user.as_ref(), &mut sess, langs);
  ctx["links"] = json!(links!(
    "tfa_action" => uri!(tfa_post),
  ));
  Rst::Template(Template::render("auth/2fa", ctx))
}

#[post("/login/2fa", format = "application/x-www-form-urlencoded", data = "<form>")]
pub fn tfa_post(form: Form<TwoFactor>, pot: PotentialUser, mut sess: Session, conn: DbConn, mut redis: Redis, mut cookies: Cookies, addr: ClientIp, l10n: L10n) -> Result<Redirect> {
  if !sess.check_token(&form.anti_csrf_token) {
    sess.add_data("error", l10n.tr("error-csrf")?);
    return Ok(Redirect::to(uri!(crate::routes::web::auth::login::get)));
  }

  let user = match pot.get(&conn)? {
    Some(u) => u,
    None => return Ok(Redirect::to("lastpage")),
  };

  let mut tfa_check = || -> Result<bool> {
    if !user.tfa_enabled() {
      return Ok(true);
    }

    let tfa_code_s = &form.code;

    match tfa_code_s.len() {
      6 => if_chain! {
        if let Some(ss) = user.shared_secret();
        if let Ok(tfa_code) = tfa_code_s.parse::<u64>();
        if !redis.exists::<_, bool>(format!("otp:{},{}", user.id(), tfa_code))?;
        if totp_raw_skew(ss).ok_or_else(|| anyhow::anyhow!("could not generate totp codes"))?.iter().any(|&x| x == tfa_code);
        then {
          redis.set_ex(format!("otp:{},{}", user.id(), tfa_code), "", 120)?;
        } else {
          return Ok(false);
        }
      },
      12 => if_chain! {
        let backup_code = diesel::delete(backup_codes::table)
          .filter(backup_codes::user_id.eq(user.id()).and(backup_codes::code.eq(tfa_code_s)))
          .get_result::<BackupCode>(&*conn)
          .optional()?;
        if backup_code.is_none();
        then {
          return Ok(false);
        }
      },
      _ => return Ok(false),
    }

    Ok(true)
  };

  if !tfa_check()? {
    let msg = match LoginAttempt::find_increment(&conn, &l10n, *addr)? {
      Some(msg) => msg,
      None => l10n.tr(("login-error", "tfa"))?,
    };
    sess.add_data("error", msg);
    return Ok(Redirect::to(uri!(tfa)));
  }

  sess.user_id = Some(user.id());

  pot.remove(&mut redis, &mut cookies)?;

  Ok(Redirect::to("lastpage"))
}

#[derive(FromForm)]
pub struct TwoFactor {
  pub anti_csrf_token: String,
  pub code: String,
}
