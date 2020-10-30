#[macro_use] extern crate rocket;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use rocket::form::Form;
use rocket::response::Redirect;
use rocket::http::{Cookie, CookieJar};
use rocket_contrib::templates::Template;

#[post("/submit", data = "<message>")]
fn submit(cookies: &CookieJar<'_>, message: Form<String>) -> Redirect {
    cookies.add(Cookie::new("message", message.into_inner()));
    Redirect::to("/")
}

#[get("/")]
fn index(cookies: &CookieJar<'_>) -> Template {
    let cookie = cookies.get("message");
    let mut context = HashMap::new();
    if let Some(ref cookie) = cookie {
        context.insert("message", cookie.value());
    }

    Template::render("index", &context)
}

#[launch]
fn rocket() -> rocket::Rocket {
    rocket::ignite().mount("/", routes![submit, index]).attach(Template::fairing())
}
