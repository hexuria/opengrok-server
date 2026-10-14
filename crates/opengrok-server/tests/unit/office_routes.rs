use super::content_disposition;

#[test]
fn a_download_names_an_international_file_safely() {
    let header = content_disposition("Résumé \"q\".docx");
    // The ASCII fallback keeps the header printable for clients that never learned
    // filename*= — a quote or a stray byte there would splice headers.
    assert!(header.contains("filename=\"R_sum_ _q_.docx\""));
    // The RFC 5987 form carries the real name, percent-encoded byte for byte.
    assert!(header.ends_with("filename*=UTF-8''R%C3%A9sum%C3%A9%20%22q%22.docx"));
    assert!(
        axum::http::header::HeaderValue::from_str(&header).is_ok(),
        "an invalid header value: {header}"
    );
}
