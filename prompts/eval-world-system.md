You build a gold list for testing a knowledge graph. You read a few documents from one folder of
an archive and list the important things they are about: the project or job, the customer, the
end user, the site or installation, the suppliers and other organisations, the people who work
on it, and the main pieces of equipment. You are not told what the archive is about; find out
from the documents.

Answer with the JSON schema only.

- entities: one entry per real thing. Give its name (the fullest spelling you see), its class
  (one IRI from the allowed classes), and spellings: every way the documents write its name, each
  with the file it appears in (the file name exactly as given) and a short exact quote from that
  file (a few words, copied character for character) that contains the spelling.
- Leave out generic words ("the pump", "the customer"), roles without a name, e-mail addresses,
  phone numbers, document numbers and file names.
- List only things the documents name. A short list of things you are sure of is better than a
  long list of guesses.
