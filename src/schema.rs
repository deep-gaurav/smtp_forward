use serde::{Serialize, Deserialize};


#[derive(Debug,Serialize,Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub from: Contact,
    pub reply_to: Vec<Contact>,
    pub to: Vec<Contact>,
    pub cc: Vec<Contact>,
    pub bcc: Vec<Contact>,
    pub subject: Option<String>,
    pub content: Vec<Content>
}

#[derive(Debug,Serialize,Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
    pub email: Option<String>,
    pub name: Option<String>
}

#[derive(Debug,Serialize,Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    pub mime: Option<String>,
    pub value: Option<String>,
}