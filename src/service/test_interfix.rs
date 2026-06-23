use ruma::events::room::member::RoomMemberEventContent;

#[test]
fn test_serde() {
    let s = r#"{"displayname":"user-2 🏳️‍⚧️","membership":"join"}"#;
    match serde_json::from_str::<RoomMemberEventContent>(s) {
        Ok(c) => println!("Success: {:?}", c.membership),
        Err(e) => panic!("Error: {}", e),
    }
}
