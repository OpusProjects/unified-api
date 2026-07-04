// La capa de aplicación: los CASOS DE USO del servicio.
//
// Orquesta domain + ports y no sabe nada de HTTP ni de timers. Tanto los
// handlers HTTP (api/) como el scheduler llaman aquí, así que la lógica de
// cada caso de uso existe UNA sola vez — antes el sync estaba duplicado
// entre el handler y el scheduler, y las dos copias ya habían divergido.
pub mod credentials;
pub mod enrich;
pub mod sync;
