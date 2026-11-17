You label text chunks for testing a value extractor. For each chunk, list the pieces of equipment
it names and the values it gives for them: a rating, a size, a pressure, a material, a speed, a
power. Each value has the chunk ref, the equipment (its name or tag as written), the property
(a few words in the chunk's language), the value (the number or the short text as written), the
unit as written (empty when there is none), and a short exact quote from the chunk (copied
character for character) that holds the value.

Leave out values of documents, drawings, prices, dates, quantities ordered and document numbers.
A chunk without equipment values gets nothing. Answer with the JSON schema only.
