Synthetic mail corpus (all addresses use reserved example domains).

The .eml files are ASCII wire messages with CRLF endings. Transfer encodings
carry the non-ASCII headers, legacy charset bodies, and binary attachments.
Git must preserve their bytes; .gitattributes marks them -text.

newsletter.eml       HTML-only, remote pixel/CSS, meta refresh, CID rewrite,
                     folded List-Unsubscribe with HTTPS and mailto targets.
alternative.eml      Plain/HTML alternatives, Cc/Bcc, Reply-To, References.
related.eml          HTML plus inline Content-ID PNG, mixed-case ID lookup.
image-attachment.eml Valid 1x1 RGBA PNG attachment, base64 transfer encoding.
calendar-request.eml REQUEST invitation, Europe/London VTIMEZONE, organizer.
calendar-cancel.eml  Same event cancelled with incremented SEQUENCE.
flowed.eml           Flowed paragraphs, quotes, space stuffing, signature.
flowed-delsp.eml     DelSp joins within words, including quoted text.
smime-signed.eml     Multipart S/MIME signature with smime.p7s attachment.
smime-encrypted.eml Enveloped-data S/MIME content.
pgp-signed.eml       PGP/MIME detached signature.
pgp-encrypted.eml    PGP/MIME version and encrypted data parts.
pgp-inline.eml       Cleartext PGP signature armor in a plain text body.
tnef.eml             Outlook winmail.dat attachment.
forwarded.eml        Attached message/rfc822 with its own headers and body.
latin1.eml           ISO-8859-1 quoted-printable and RFC 2047 Q headers.
shift-jis.eml        Shift_JIS base64 body and RFC 2047 B headers.
delivery-failure.eml Multipart/report, delivery-status fields, original mail.

Cryptographic payloads and TNEF data are deliberately inert placeholders:
these fixtures exercise MIME recognition/notices, not signature validation,
decryption or TNEF extraction. Both PNGs are fully decodable images.
The newsletter deliberately references an unavailable CID logo to exercise
display-only rewriting even when a sender omits the corresponding image.
