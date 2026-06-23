use ruma::events::room::member::RoomMemberEventContent;

fn main() {
    let s = r#"{"displayname":"user-2 🏳️‍⚧️","membership":"join"}"#;
    match serde_json::from_str::<RoomMemberEventContent>(s) {
        Ok(c) => println!("Success: {:?}", c.membership),
        Err(e) => println!("Error: {}", e),
    }
}
